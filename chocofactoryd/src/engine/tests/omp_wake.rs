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
    start_wake_on(WAKE_YAML, steps, timers).await
}

/// `start_wake` for a workflow other than `WAKE_YAML` (its stage must be
/// `review` and its terminals `finished` / `bounced`).
async fn start_wake_on(yaml: &str, steps: Value, timers: TurnTimers) -> WakeRun {
    let dir = tempdir();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let pool = connect(&dir.join("choco.db")).await.unwrap();
    let script = OmpScript::new(&dir, steps, &[]);
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
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
    // The outcome event is appended after the task transition, so wait for it.
    wait_until_turn_outcome_event(&run.pool, &run.task_id).await;
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

/// A closing message array as omp sends it: an earlier clean message, a tool
/// result, the run's last assistant message and a trailing user message.
fn multi_message_end(first: &str, last: &str) -> Value {
    let assistant = |reason: &str| {
        let mut message = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "x"}],
            "stopReason": reason,
        });
        if reason == "error" {
            message["errorMessage"] = json!("provider fell over mid-answer");
        }
        message
    };
    json!({"op": "agent_end", "messages": "raw", "raw": [
        assistant(first),
        {"role": "toolResult", "toolName": "bash", "isError": false},
        assistant(last),
        {"role": "user", "content": "go on"},
    ]})
}

/// The last assistant message of the closing array decides: an error after
/// a clean message is not applied, a clean message after an error is.
#[tokio::test]
async fn omp_the_last_message_of_a_wakes_end_decides_whether_it_applies() {
    let failed = start_wake(
        wake_steps(
            json!(true),
            changes_requested(),
            vec![
                multi_message_end("stop", "error"),
                json!({"op": "session_settled"}),
            ],
        ),
        timers(),
    )
    .await;
    failed.script.wait_log("op:gate").await;
    failed.script.release("wake");
    assert_not_applied(&failed, "an error after a clean message").await;

    let clean = start_wake(
        wake_steps(
            json!(true),
            changes_requested(),
            vec![
                multi_message_end("error", "stop"),
                json!({"op": "session_settled"}),
            ],
        ),
        timers(),
    )
    .await;
    clean.script.wait_log("op:gate").await;
    clean.script.release("wake");
    assert_applied_once(&clean).await;
}

fn yaml_with_stage(stage: &str) -> String {
    format!(
        "name: omp-wake\nroles:\n  reviewer:\n    cli: omp\n    model: openai-codex/gpt-5.6-terra\nstages:\n  review:\n    kind: agent_turn\n    role: reviewer\n{stage}\n  finished:\n    kind: terminal\n"
    )
}

fn done_report() -> Value {
    json!({"outcome": "done", "summary": SUMMARY})
}

/// `capture: text`: the reply of the wake's own run is what is stored.
#[tokio::test]
async fn omp_a_wake_on_a_text_capture_stage_stores_its_final_reply() {
    let tail = vec![
        json!({"op": "assistant", "text": "wake-reply-5d2e: all done"}),
        json!({"op": "agent_end"}),
        json!({"op": "session_settled"}),
    ];
    let yaml = yaml_with_stage("    capture: text\n    on: { done: finished }");
    let run = start_wake_on(
        &yaml,
        wake_steps(json!(true), done_report(), tail),
        timers(),
    )
    .await;
    run.script.wait_log("op:gate").await;
    assert!(run.outcomes().await.is_empty());
    run.script.release("wake");
    wait_until_stage(&run.pool, &run.task_id, "finished").await;
    wait_until_task_status(&run.pool, &run.task_id, "closed").await;
    assert_eq!(
        payload_of(&run.pool, &run.task_id).await["stages"]["review"],
        json!("wake-reply-5d2e: all done"),
        "the wake's own final reply, not the first run's: {}",
        run.describe().await
    );
    assert_eq!(run.script.prompts_received(), 1);
    run.script.wait_log("eof").await;
}

/// No `capture:`: the accepted `done` advances the stage once and nothing is
/// stored for it.
#[tokio::test]
async fn omp_a_wake_on_a_stage_without_capture_advances_and_stores_nothing() {
    let yaml = yaml_with_stage("    on: { done: finished }");
    let run = start_wake_on(
        &yaml,
        wake_steps(json!(true), done_report(), clean_finish()),
        timers(),
    )
    .await;
    run.script.wait_log("op:gate").await;
    run.script.release("wake");
    wait_until_stage(&run.pool, &run.task_id, "finished").await;
    wait_until_task_status(&run.pool, &run.task_id, "closed").await;
    let stored = payload_of(&run.pool, &run.task_id).await["stages"]["review"].clone();
    assert!(stored.is_null(), "nothing captured: {stored}");
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![
            ("review".to_string(), Value::Null),
            ("finished".to_string(), json!("done")),
        ]
    );
    // Without `capture: json` no turn outcome is recorded; the single
    // advance on `done` above is the one application.
    assert!(run.outcomes().await.is_empty());
    assert_eq!(run.script.prompts_received(), 1);
    run.script.wait_log("eof").await;
}

/// A chat stage (empty `on:`) never concludes: the wake's end is recorded,
/// the session stays open for the next message and nothing is applied.
#[tokio::test]
async fn omp_a_wake_in_a_chat_stage_leaves_the_conversation_open() {
    let steps = json!([
        {"op": "pid"},
        {"op": "agent_start"},
        {"op": "assistant", "text": "started the service"},
        {"op": "agent_end", "terminal": false, "awaiting": true},
        {"op": "prompt_result", "settled": false},
        {"op": "gate", "name": "wake"},
        {"op": "agent_start"},
        {"op": "assistant", "text": "the service finished"},
        {"op": "agent_end"},
        {"op": "session_settled"},
    ]);
    let yaml = yaml_with_stage("    on: {}");
    let run = start_wake_on(&yaml, steps, timers()).await;
    run.script.wait_log("op:gate").await;
    run.script.release("wake");
    run.script.wait_log("script_done").await;
    let session = run.session().await;
    crate::test_support::wait_until("the wake's own end to be recorded", || async {
        let ends = events::list_for_session(&run.pool, &session.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::TurnCompleted)
            .count();
        if ends == 2 {
            Ok(())
        } else {
            Err(format!("{ends} ends so far"))
        }
    })
    .await;
    let session = sessions::get(&run.pool, &session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        session.status,
        SessionStatus::Active,
        "{}",
        run.describe().await
    );
    assert!(run.outcomes().await.is_empty());
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![("review".to_string(), Value::Null)]
    );
    assert_eq!(
        run.script.prompts_received(),
        1,
        "no nudge or second prompt"
    );
    // Cancelling is what ends a standing conversation. Wait for the session
    // to end and the child to be reaped, so nothing is still tearing down
    // when the test's directories go.
    let pid = run.script.pid();
    run.engine.cancel_task(&run.task_id, false).await.unwrap();
    crate::test_support::wait_until("the chat session to end as cancelled", || async {
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
    let ended = sessions::get(&run.pool, &session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended.end_reason, Some(SessionEndReason::Cancelled));
    crate::test_support::wait_until("the chat child to be reaped", || async {
        if process_alive(pid) {
            Err(format!("pid {pid} still alive"))
        } else {
            Ok(())
        }
    })
    .await;
}

// ---- the wake's cleanup: verified cleanup lets the accepted outcome apply ----

/// A job pid file outside any test directory; dropping it kills the job and
/// removes the file.
struct JobFile(PathBuf);

impl JobFile {
    fn new() -> Self {
        JobFile(std::env::temp_dir().join(format!("choco-wake-job-{}.pid", uuid::Uuid::new_v4())))
    }

    fn pid(&self) -> u32 {
        fs::read_to_string(&self.0).unwrap().trim().parse().unwrap()
    }
}

/// The pid in a job's pid file, for a test whose fixture owns the file's
/// cleanup elsewhere.
fn job_pid(file: &Path) -> u32 {
    fs::read_to_string(file).unwrap().trim().parse().unwrap()
}

impl Drop for JobFile {
    fn drop(&mut self) {
        drop(crate::test_support::KillPidFileOnDrop(self.0.clone()));
        let _ = fs::remove_file(&self.0);
    }
}

/// `wake_steps` that first start an owned background job of the session.
fn wake_steps_with_job(job_file: &Path, tail: Vec<Value>) -> Value {
    let mut steps = clean_finish();
    steps.extend(tail);
    let mut steps = wake_steps(json!("ABSENT"), changes_requested(), steps);
    steps
        .as_array_mut()
        .unwrap()
        .insert(1, json!({"op": "spawn_job", "pid_file": job_file}));
    steps
}

/// The wake as a named service reaches the engine: a report-less yield, an
/// autonomous wake that reports and ends cleanly, and the CLI exiting on its
/// own afterwards with the job the session started still running. The tail
/// sweep kills that job, the cleanup is verified, and the report applies once
/// without a nudge or a second prompt.
#[tokio::test]
async fn the_named_service_tail_cleanup_still_advances() {
    let job = JobFile::new();
    let run = start_wake(wake_steps_with_job(&job.0, vec![]), timers()).await;
    run.script.wait_log("op:gate").await;
    assert!(run.outcomes().await.is_empty());
    run.script.release("wake");

    let task = wait_until_task_settled(&run.pool, &run.task_id).await;
    assert_eq!(task.status, "closed", "{}", run.describe().await);
    let outcome = wait_until_turn_outcome_event(&run.pool, &run.task_id).await;
    assert_eq!(outcome["applied"], true, "{outcome}");
    assert_eq!(run.outcomes().await.len(), 1, "{}", run.describe().await);
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![
            ("review".to_string(), Value::Null),
            ("bounced".to_string(), json!("changes_requested")),
        ]
    );
    let session = run.session().await;
    crate::test_support::wait_until("the session's final write", || async {
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
    assert_eq!(
        (session.status, session.end_reason, session.cleanup),
        (
            SessionStatus::Idle,
            None,
            Some(chocofactory_core::models::SessionCleanup::Verified)
        ),
        "{}",
        run.describe().await
    );
    wait_until_pid_gone(job.pid()).await;
    assert_eq!(
        run.script.prompts_received(),
        1,
        "no nudge or second prompt"
    );
    let kinds = run.notes().await;
    for unwanted in ["nudge", "no_report"] {
        assert!(!kinds.iter().any(|k| k == unwanted), "{kinds:?}");
    }
}

const MIXED_YAML: &str = "name: mixed-review\nworktree: true\nroles:\n  claude_reviewer:\n    cli: claude\n    model: sonnet\n    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\n  omp_reviewer:\n    cli: omp\n    model: openai-codex/gpt-5.6-terra\n    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n  panel:\n    kind: parallel\n    branches:\n      claude_branch: { kind: agent_turn, role: claude_reviewer, capture: json, results: [approved, changes_requested] }\n      omp_branch: { kind: agent_turn, role: omp_reviewer, capture: json, results: [approved, changes_requested] }\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

const CLAUDE_SUMMARY: &str = "claude-summary-1b2c: the parser is fine";

/// A two-adapter review group: an ordinary Claude branch that reports and
/// finishes at once, and an omp branch that is the named-service wake held
/// through grace with a job it started still running.
struct MixedRun {
    pool: SqlitePool,
    _engine: Arc<WorkflowEngine>,
    task_id: String,
    script: OmpScript,
    armed: Arc<std::sync::atomic::AtomicBool>,
    later_reads: Arc<std::sync::atomic::AtomicUsize>,
    omp_job: PathBuf,
    _dir: TempDir,
    _repo: TempDir,
}

impl Drop for MixedRun {
    fn drop(&mut self) {
        // Before the directories go, so the pid file is still readable.
        drop(crate::test_support::KillPidFileOnDrop(self.omp_job.clone()));
    }
}

/// Starts the group. The injected cleanup fault is aimed at the omp session
/// only: it is armed by the test after the Claude branch has finished.
async fn start_mixed(cleanup: Cleanup) -> MixedRun {
    let dir = tempdir();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let pool = connect(&dir.join("choco.db")).await.unwrap();
    let omp_job = dir.join("omp-job.pid");
    let script = OmpScript::new(
        &dir,
        wake_steps_with_job(&omp_job, vec![json!({"op": "sleep", "seconds": 60})]),
        &[],
    );
    let claude_binary = named_script_binary(
        &dir,
        "fake-claude-mixed",
        json!([
            {"op": "read_turn"},
            {"op": "report", "outcome": "approved", "summary": CLAUDE_SUMMARY},
            {"op": "result"},
        ]),
    );
    let def = Arc::new(WorkflowDefinition::parse(MIXED_YAML, &dir).unwrap());
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
    let registry = Registry::new(vec![
        Arc::new(ClaudeAdapter::with_binary(&claude_binary)),
        Arc::new(OmpAdapter::with_binary(
            &script.binary,
            dir.join("omp-state"),
        )),
    ]);
    let events_notify = Arc::new(Notify::new());
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let later_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = SessionManager::with_proc_reader(
        pool.clone(),
        registry,
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
        TurnTimers {
            grace: StdDuration::from_secs(3),
            ..timers()
        },
        gated_cleanup_reader(cleanup, Arc::clone(&later_reads), Arc::clone(&armed)),
    );
    let engine = WorkflowEngine::new(
        pool.clone(),
        manager,
        PathBuf::from("."),
        None,
        events_notify,
    );
    engine.start_task(&task_id, &def, Some("go")).await.unwrap();
    MixedRun {
        pool,
        _engine: engine,
        task_id,
        script,
        armed,
        later_reads,
        omp_job,
        _dir: dir,
        _repo: repo,
    }
}

impl MixedRun {
    async fn outcomes(&self) -> Vec<Value> {
        turn_outcomes(&self.pool, &self.task_id)
            .await
            .into_iter()
            .map(|e| e.payload)
            .collect()
    }

    async fn payload(&self) -> Value {
        workflow_state::get(&self.pool, &self.task_id)
            .await
            .unwrap()
            .unwrap()
            .payload
    }

    async fn branch_session(&self, branch: &str) -> Session {
        crate::test_support::wait_until(&format!("the {branch} session"), || async {
            runs_for_stage(&self.pool, &self.task_id, branch)
                .await
                .into_iter()
                .next()
                .ok_or_else(|| "no session yet".to_string())
        })
        .await
    }

    /// Runs the group up to the point the scenario is about: the omp branch
    /// has yielded without a report and is waiting on its background work,
    /// while the Claude branch has finished with its own result. Returns the
    /// Claude branch's session as the group's join must leave it.
    async fn until_claude_is_done_and_omp_waits(&self) -> Session {
        self.script.wait_log("op:gate").await;
        crate::test_support::wait_until("the claude branch to be done", || async {
            let payload = self.payload().await;
            if payload["parallel"]["branches"]["claude_branch"]["state"] == "done" {
                Ok(())
            } else {
                Err(payload.to_string())
            }
        })
        .await;
        let claude =
            crate::test_support::wait_until("the claude session's final write", || async {
                let s = self.branch_session("claude_branch").await;
                if s.status == SessionStatus::Active {
                    Err(format!("{s:?}"))
                } else {
                    Ok(s)
                }
            })
            .await;
        // The omp branch is still running with no result and the group has not
        // joined: the wake has not happened yet.
        let payload = self.payload().await;
        assert_eq!(
            payload["parallel"]["branches"]["omp_branch"]["state"], "running",
            "{payload}"
        );
        assert_eq!(
            payload["stages"]["claude_branch"]["summary"],
            CLAUDE_SUMMARY
        );
        assert!(payload["stages"].get("omp_branch").is_none(), "{payload}");
        assert_eq!(
            tasks::get(&self.pool, &self.task_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        assert_eq!(self.script.prompts_received(), 1);
        claude
    }
}

/// Both branches of a review group, one of them an omp wake that needs the
/// grace kill and has a job left running: with verified cleanup the wake's
/// accepted verdict is the branch's result, the ordinary Claude sibling's
/// result and session are untouched, the group joins once after both, and
/// each branch is applied once without a second prompt.
#[tokio::test]
async fn a_mixed_review_group_applies_the_cleaned_wake_once() {
    use chocofactory_core::models::SessionCleanup;
    let run = start_mixed(Cleanup::Clean).await;
    let claude_before = run.until_claude_is_done_and_omp_waits().await;
    run.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    run.script.release("wake");

    let task = wait_until_task_settled(&run.pool, &run.task_id).await;
    assert_eq!(
        task.status,
        "closed",
        "{:?}; payload {}",
        task.stuck_reason,
        run.payload().await
    );
    let payload = run.payload().await;
    assert_eq!(
        payload["stages"]["omp_branch"]["outcome"],
        "changes_requested"
    );
    assert_eq!(payload["stages"]["omp_branch"]["summary"], SUMMARY);
    assert_eq!(
        payload["stages"]["claude_branch"]["summary"],
        CLAUDE_SUMMARY
    );
    // Each branch finished once, as done; the join then left the group.
    let finished: Vec<_> = events::list_for_task(&run.pool, &run.task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EventType::BranchFinished)
        .map(|e| (e.payload["branch"].clone(), e.payload["state"].clone()))
        .collect();
    assert_eq!(finished.len(), 2, "{finished:?}");
    for branch in ["claude_branch", "omp_branch"] {
        assert!(
            finished.contains(&(json!(branch), json!("done"))),
            "{branch}: {finished:?}"
        );
    }
    // One group join, after both branch results.
    assert_eq!(
        stage_trail(&run.pool, &run.task_id)
            .await
            .iter()
            .filter(|(stage, _)| stage == "finished")
            .count(),
        1
    );
    let omp = run.branch_session("omp_branch").await;
    let omp = crate::test_support::wait_until("the omp session's final write", || async {
        let s = sessions::get(&run.pool, &omp.id).await.unwrap().unwrap();
        if s.status == SessionStatus::Active {
            Err("still active".to_string())
        } else {
            Ok(s)
        }
    })
    .await;
    assert_eq!(
        (omp.status, omp.end_reason, omp.cleanup),
        (SessionStatus::Idle, None, Some(SessionCleanup::Verified))
    );
    wait_until_pid_gone(job_pid(&run.omp_job)).await;
    // The Claude sibling's session is exactly what it was before the wake.
    let claude_after = sessions::get(&run.pool, &claude_before.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            claude_after.status,
            claude_after.end_reason,
            claude_after.cleanup
        ),
        (
            claude_before.status,
            claude_before.end_reason,
            claude_before.cleanup
        )
    );
    // One application per branch, the wake's after its cleanup.
    let outcomes = crate::test_support::wait_until("both turn_outcome events", || async {
        let all = run.outcomes().await;
        if all.len() >= 2 {
            Ok(all)
        } else {
            Err(format!("{all:?}"))
        }
    })
    .await;
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    for branch in ["claude_branch", "omp_branch"] {
        let of: Vec<_> = outcomes.iter().filter(|o| o["stage"] == branch).collect();
        assert_eq!(of.len(), 1, "{branch}: {outcomes:?}");
        assert_eq!(of[0]["applied"], true, "{branch}: {outcomes:?}");
    }
    assert_eq!(
        run.script.prompts_received(),
        1,
        "no nudge or second prompt"
    );
    assert_eq!(
        runs_for_stage(&run.pool, &run.task_id, "omp_branch")
            .await
            .len(),
        1
    );
}

/// The same group when the omp branch's cleanup cannot be verified (a
/// survivor the last check still sees, or one failed process-table read): its
/// report stays on record but the branch fails as lingered, the Claude
/// sibling's result and session survive, and the group parks once nothing is
/// running.
async fn assert_unverified_wake_fails_only_its_branch(name: &str, cleanup: Cleanup) {
    use chocofactory_core::models::SessionCleanup;
    let run = start_mixed(cleanup).await;
    let claude_before = run.until_claude_is_done_and_omp_waits().await;
    run.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    run.script.release("wake");

    let task = wait_until_task_settled(&run.pool, &run.task_id).await;
    assert_eq!(task.status, "stuck", "{name}: {:?}", task.stuck_reason);
    let omp = crate::test_support::wait_until("the omp session's final write", || async {
        let s = run.branch_session("omp_branch").await;
        if s.status == SessionStatus::Active {
            Err("still active".to_string())
        } else {
            Ok(s)
        }
    })
    .await;
    assert_eq!(
        (omp.status, omp.end_reason, omp.cleanup),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::Lingered),
            Some(SessionCleanup::Unverified)
        ),
        "{name}"
    );
    wait_until_pid_gone(job_pid(&run.omp_job)).await;
    if name == "a failed read" {
        assert!(
            run.later_reads.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "later reads must have worked"
        );
    }
    let payload = run.payload().await;
    assert_eq!(
        payload["parallel"]["branches"]["omp_branch"]["state"], "failed",
        "{name}: {payload}"
    );
    assert!(
        payload["stages"].get("omp_branch").is_none(),
        "{name}: {payload}"
    );
    // The sibling's result and session are exactly as they were, and the
    // group did not advance.
    assert_eq!(
        payload["stages"]["claude_branch"]["summary"], CLAUDE_SUMMARY,
        "{name}"
    );
    assert_eq!(
        payload["parallel"]["branches"]["claude_branch"]["state"], "done",
        "{name}"
    );
    let claude_after = sessions::get(&run.pool, &claude_before.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claude_after.end_reason, claude_before.end_reason, "{name}");
    assert_eq!(claude_after.status, claude_before.status, "{name}");
    let state = workflow_state::get(&run.pool, &run.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.current_stage, "panel", "{name}");
    assert!(
        stage_trail(&run.pool, &run.task_id)
            .await
            .iter()
            .all(|(stage, _)| stage != "finished"),
        "{name}: the group must not join"
    );
    // The wake's report is still readable on its session's events.
    let reported = events::list_for_session(&run.pool, &omp.id)
        .await
        .unwrap()
        .into_iter()
        .any(|e| e.payload.to_string().contains(SUMMARY));
    assert!(reported, "{name}: the accepted report was lost");
    // Only the sibling's verdict was ever applied: the wake's is recorded
    // as not applied, or not recorded at all.
    // The sibling's own outcome event is appended after its branch is
    // settled, so wait for it rather than read whatever exists at the moment
    // the group parked.
    let outcomes = crate::test_support::wait_until("the Claude sibling's turn_outcome", || async {
        let all = run.outcomes().await;
        if all.iter().any(|o| o["stage"] == "claude_branch") {
            Ok(all)
        } else {
            Err(format!("{all:?}"))
        }
    })
    .await;
    let applied_of = |branch: &str| {
        outcomes
            .iter()
            .filter(|o| o["stage"] == branch)
            .map(|o| o["applied"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(applied_of("claude_branch"), vec![json!(true)], "{name}");
    assert!(
        !applied_of("omp_branch").contains(&json!(true)),
        "{name}: {outcomes:?}"
    );
}

#[tokio::test]
async fn a_mixed_review_group_fails_only_the_wake_branch_after_a_survivor() {
    assert_unverified_wake_fails_only_its_branch("a survivor", Cleanup::SurvivorAlive).await;
}

#[tokio::test]
async fn a_mixed_review_group_fails_only_the_wake_branch_after_a_failed_read() {
    assert_unverified_wake_fails_only_its_branch("a failed read", Cleanup::OneFailedRead).await;
}
