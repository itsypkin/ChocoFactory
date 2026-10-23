//! Pausing and resuming a task (#295): the statuses, the hold, the stops and
//! what `resume` starts again. Uses the queue tests' harness, whose fake
//! agent holds a turn until the test releases it.

use super::queue::{Ending, Q, held};
use super::*;

/// agent turn -> shell -> terminal: the shell stage is what a pause holds.
const PFLOW: &str = "name: pflow\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: { done: checks }\n  checks:\n    kind: shell\n    command: \"echo ok\"\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

/// A poll that never matches, with a short budget.
const POLL_FLOW: &str = "name: ppoll\nstages:\n  watch:\n    kind: poll\n    command: \"echo PENDING\"\n    interval: 1s\n    timeout: 3s\n    outcomes:\n      - match: \"SUCCESS\"\n        then: green\n    on: { green: finished, timeout: stalled }\n  finished:\n    kind: terminal\n  stalled:\n    kind: human_gate\n    on: { resumed: finished }\n";

/// A gate, then a shell stage, then the end.
const GATE_FLOW: &str = "name: pgate\nstages:\n  review:\n    kind: human_gate\n    on: { resumed: work }\n  work:\n    kind: shell\n    command: \"echo ok\"\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

/// A gate whose reply reaches the terminal stage at once.
const GATE_END_FLOW: &str = "name: pgateend\nstages:\n  review:\n    kind: human_gate\n    on: { resumed: finished }\n  finished:\n    kind: terminal\n";

async fn create_flow(q: &Q, yaml: &str, file: &str, name: &str) -> Task {
    let path = q.dir.join(file);
    fs::write(&path, yaml).unwrap();
    q.engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(path),
            name,
            name,
            json!({}),
            None,
            &[],
        )
        .await
        .unwrap()
}

/// A shell flow whose command holds until the test releases it.
fn shell_flow(q: &Q) -> String {
    format!(
        "name: pshell\nstages:\n  build:\n    kind: shell\n    command: \"touch '{d}/started-sh'; while [ ! -f '{d}/release-sh' ]; do sleep 0.1; done\"\n    on: {{ done: after }}\n  after:\n    kind: shell\n    command: \"echo ok\"\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n",
        d = q.dir.display()
    )
}

async fn task_of(q: &Q, id: &str) -> Task {
    tasks::get(&q.pool, id).await.unwrap().unwrap()
}

/// `paused_at` is set exactly while the status is `pausing` or `paused`.
async fn assert_paused_at_invariant(q: &Q, id: &str) {
    let task = task_of(q, id).await;
    assert_eq!(
        task.paused_at.is_some(),
        is_pause_status(&task.status),
        "paused_at {:?} with status {}",
        task.paused_at,
        task.status
    );
}

async fn current_stage(q: &Q, id: &str) -> String {
    workflow_state::get(&q.pool, id)
        .await
        .unwrap()
        .unwrap()
        .current_stage
}

async fn wait_runner_gone(q: &Q, id: &str) {
    crate::test_support::wait_until("the detached runner to go", || async {
        if q.engine.has_detached_runner(id) {
            Err("still registered".to_string())
        } else {
            Ok(())
        }
    })
    .await
}

fn plain_script(q: &Q, name: &str) {
    // A resumed turn is told it was interrupted instead of getting its old
    // prompt, so it follows a plain script.
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!(held(&q.dir, name, Ending::Report)).to_string(),
    )
    .unwrap();
}

// ---- test 1: graceful pause in a coding stage ---------------------------------

#[tokio::test]
async fn a_graceful_pause_lets_the_turn_finish_holds_the_next_stage_and_resume_starts_it() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;

    let outcome = q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(outcome.status, "pausing");
    assert_eq!(outcome.mode, "graceful");
    assert_eq!(outcome.running, 1);
    assert_eq!(q.status(&a.id).await, "pausing");
    assert_paused_at_invariant(&q, &a.id).await;
    let paused_at = task_of(&q, &a.id).await.paused_at.unwrap();

    q.release("job-a");
    q.wait_status(&a.id, "paused").await;
    // `pausing -> paused` leaves `paused_at` as the pause request set it.
    assert_eq!(task_of(&q, &a.id).await.paused_at, Some(paused_at));
    assert_paused_at_invariant(&q, &a.id).await;
    // The next stage is entered but held: no session, runner, queue row or
    // command output, and a `stage_held` event.
    assert_eq!(current_stage(&q, &a.id).await, "checks");
    assert!(!q.engine.has_detached_runner(&a.id));
    assert!(q.rows().await.is_empty());
    assert_eq!(q.sessions(&a.id).await.len(), 1);
    assert!(
        q.queue_events(&a.id, EventType::ShellOutput)
            .await
            .is_empty()
    );
    let held_events = q.queue_events(&a.id, EventType::StageHeld).await;
    assert_eq!(held_events.len(), 1);
    assert_eq!(held_events[0]["stage"], "checks");
    assert_eq!(q.queue_events(&a.id, EventType::TaskPausing).await.len(), 1);
    assert_eq!(q.queue_events(&a.id, EventType::TaskPaused).await.len(), 1);

    let resumed = q.engine.resume_task(&a.id).await.unwrap();
    assert_eq!(resumed.started, vec!["checks".to_string()]);
    q.wait_status(&a.id, "closed").await;
    assert_paused_at_invariant(&q, &a.id).await;
    assert_eq!(q.queue_events(&a.id, EventType::TaskResumed).await.len(), 1);
}

// ---- tests 2, 5, 6: a poll ------------------------------------------------------

#[tokio::test]
async fn pausing_a_poll_is_paused_at_once_and_resume_polls_again() {
    let q = Q::new(None).await;
    let a = create_flow(&q, POLL_FLOW, "ppoll.yaml", "poll").await;
    assert!(q.engine.has_detached_runner(&a.id));
    let outcome = q.engine.pause_task(&a.id, false).await.unwrap();
    // Nothing of the task's is running: paused when the call returns.
    assert_eq!(outcome.status, "paused");
    assert_eq!(q.status(&a.id).await, "paused");
    assert!(
        !q.engine.has_detached_runner(&a.id),
        "the poll runner is gone"
    );
    assert_paused_at_invariant(&q, &a.id).await;

    q.engine.resume_task(&a.id).await.unwrap();
    assert_eq!(q.status(&a.id).await, "open");
    assert!(q.engine.has_detached_runner(&a.id), "polling again");
    assert_paused_at_invariant(&q, &a.id).await;
    q.engine.cancel_task(&a.id, false).await.unwrap();
}

#[tokio::test]
async fn no_timeout_fires_while_paused_and_the_budget_left_is_kept() {
    let q = Q::new(None).await;
    let a = create_flow(&q, POLL_FLOW, "ppoll.yaml", "poll").await;
    let window = |state: &chocofactory_core::models::WorkflowState| {
        let w = &state.payload["poll_window"];
        let at = |key: &str| -> DateTime<Utc> {
            DateTime::parse_from_rfc3339(w[key].as_str().unwrap())
                .unwrap()
                .with_timezone(&Utc)
        };
        (at("entered_at"), at("deadline"))
    };
    let before = workflow_state::get(&q.pool, &a.id).await.unwrap().unwrap();
    let (entered, deadline) = window(&before);
    assert_eq!(deadline - entered, chrono::Duration::seconds(3));

    q.engine.pause_task(&a.id, false).await.unwrap();
    let paused_at = task_of(&q, &a.id).await.paused_at.unwrap();
    // Longer than the poll's whole timeout.
    tokio::time::sleep(StdDuration::from_secs(4)).await;
    assert_eq!(q.status(&a.id).await, "paused");
    assert_eq!(current_stage(&q, &a.id).await, "watch", "no timeout fired");

    q.engine.resume_task(&a.id).await.unwrap();
    let resumed_at = Utc::now();
    let after = workflow_state::get(&q.pool, &a.id).await.unwrap().unwrap();
    let (entered2, deadline2) = window(&after);
    assert_eq!(deadline2 - entered2, chrono::Duration::seconds(3));
    // What is left is the budget minus the time used before the pause.
    let left_before_pause = deadline - paused_at;
    let left_after_resume = deadline2 - resumed_at;
    let drift = (left_before_pause - left_after_resume)
        .num_milliseconds()
        .abs();
    assert!(drift < 1500, "budget left changed by {drift} ms");
    // And the poll goes on to time out in its own good time.
    q.engine.cancel_task(&a.id, false).await.unwrap();
}

// ---- test 3: pause --now --------------------------------------------------------

#[tokio::test]
async fn pause_now_stops_the_turn_and_resume_continues_the_same_session() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    let old = q.sessions(&a.id).await.remove(0);
    assert_eq!(old.end_reason, Some(SessionEndReason::Paused));
    assert_eq!(old.status, SessionStatus::Exited);
    // Never stuck on the way.
    let task = task_of(&q, &a.id).await;
    assert!(task.stuck_reason.is_none());
    assert_paused_at_invariant(&q, &a.id).await;
    assert!(q.queue_events(&a.id, EventType::Error).await.is_empty());
    assert_eq!(
        q.queue_events(&a.id, EventType::TaskPausing).await[0]["mode"],
        "now"
    );

    if old.adapter_session_id.is_none() {
        sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
            .await
            .unwrap();
    }
    plain_script(&q, "job-a");
    q.release("job-a");
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert_eq!(outcome.resumed, vec!["coding".to_string()]);
    q.wait_status(&a.id, "closed").await;
    let all = q.sessions(&a.id).await;
    assert_eq!(all.len(), 2);
    let new = all.iter().find(|s| s.id != old.id).unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
}

#[tokio::test]
async fn resume_starts_fresh_when_the_paused_session_cannot_be_resumed() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    let old = q.sessions(&a.id).await.remove(0);
    // The session never reported an id to resume.
    sqlx::query("UPDATE sessions SET adapter_session_id = NULL WHERE id = ?")
        .bind(&old.id)
        .execute(&q.pool)
        .await
        .unwrap();
    plain_script(&q, "job-a");
    q.release("job-a");
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert!(outcome.resumed.is_empty());
    assert_eq!(outcome.started, vec!["coding".to_string()]);
    q.wait_status(&a.id, "closed").await;
    let new = q
        .sessions(&a.id)
        .await
        .into_iter()
        .find(|s| s.id != old.id)
        .unwrap();
    assert_eq!(new.resumed_from, None);
}

// ---- test 15: a shell stage --------------------------------------------------------

#[tokio::test]
async fn a_graceful_pause_waits_for_the_shell_command_and_now_kills_it() {
    let q = Q::new(None).await;
    let a = create_flow(&q, &shell_flow(&q), "pshell.yaml", "sh").await;
    crate::test_support::wait_until("the command to start", || async {
        if q.dir.join("started-sh").exists() {
            Ok(())
        } else {
            Err("not started".to_string())
        }
    })
    .await;
    let outcome = q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(outcome.status, "pausing");
    assert!(
        q.engine.has_detached_runner(&a.id),
        "shell runners are kept"
    );
    assert_eq!(q.status(&a.id).await, "pausing");
    // `now` on the pausing task kills the command; `paused` follows.
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    assert!(!q.engine.has_detached_runner(&a.id));
    assert_eq!(current_stage(&q, &a.id).await, "build", "not advanced");
    // Resume runs it again from the start.
    fs::remove_file(q.dir.join("started-sh")).unwrap();
    fs::write(q.dir.join("release-sh"), "").unwrap();
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert_eq!(outcome.started, vec!["build".to_string()]);
    q.wait_status(&a.id, "closed").await;
    assert!(q.dir.join("started-sh").exists(), "it ran again");
}

#[tokio::test]
async fn a_shell_command_that_ends_under_a_graceful_pause_settles_it_after_the_runner_is_gone() {
    let q = Q::new(None).await;
    let a = create_flow(&q, &shell_flow(&q), "pshell.yaml", "sh").await;
    crate::test_support::wait_until("the command to start", || async {
        if q.dir.join("started-sh").exists() {
            Ok(())
        } else {
            Err("not started".to_string())
        }
    })
    .await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    fs::write(q.dir.join("release-sh"), "").unwrap();
    q.wait_status(&a.id, "paused").await;
    wait_runner_gone(&q, &a.id).await;
    // The command finished and its result was applied; the next stage is
    // entered but held.
    assert_eq!(current_stage(&q, &a.id).await, "after");
    assert!(q.queue_events(&a.id, EventType::ShellOutput).await.len() == 1);
    assert_paused_at_invariant(&q, &a.id).await;
}

// ---- test 4: restart -------------------------------------------------------------

#[tokio::test]
async fn a_paused_task_stays_paused_across_a_restart_and_no_poll_restarts() {
    let q = Q::new(None).await;
    let a = create_flow(&q, POLL_FLOW, "ppoll.yaml", "poll").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    let paused_at = task_of(&q, &a.id).await.paused_at;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&a.id).await, "paused");
    assert_eq!(task_of(&q, &a.id).await.paused_at, paused_at);
    assert!(!next.has_detached_runner(&a.id), "no poll was restarted");
}

#[tokio::test]
async fn a_pausing_task_is_paused_after_a_crash_and_resume_continues_its_session() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(q.status(&a.id).await, "pausing");
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&a.id).await, "paused");
    assert_paused_at_invariant(&q, &a.id).await;
    let old = q.sessions(&a.id).await.remove(0);
    assert_eq!(old.end_reason, Some(SessionEndReason::DaemonStopped));
    if old.adapter_session_id.is_none() {
        sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
            .await
            .unwrap();
    }
    plain_script(&q, "job-a");
    let outcome = next.resume_task(&a.id).await.unwrap();
    assert_eq!(outcome.resumed, vec!["coding".to_string()]);
    q.release("job-a");
    q.wait_status(&a.id, "closed").await;
    let new = q
        .sessions(&a.id)
        .await
        .into_iter()
        .find(|s| s.id != old.id)
        .unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
}

#[tokio::test]
async fn a_graceful_stop_leaves_a_pausing_turn_paused_not_stuck() {
    // The watcher sees `daemon_stopped` on a pausing task and handles it like
    // a pause: not parked, and `settle_pause` follows.
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    let session = q.sessions(&a.id).await.remove(0);
    q.engine
        .session_manager
        .shutdown(StdDuration::from_secs(10))
        .await;
    q.wait_status(&a.id, "paused").await;
    let after = sessions::get(&q.pool, &session.id).await.unwrap().unwrap();
    assert_eq!(after.end_reason, Some(SessionEndReason::DaemonStopped));
    assert!(task_of(&q, &a.id).await.stuck_reason.is_none());
}

#[tokio::test]
async fn the_sweep_ignores_a_session_from_an_earlier_entry_of_a_pausing_stage() {
    // A task left `pausing` whose stage was entered (held) after its last
    // session ended: that session belongs to an earlier lap and was not cut
    // off, so the sweep neither records nor judges it.
    let q = Q::new(None).await;
    let task = q.task_at("coding", "open").await;
    let old = sessions::create(
        &q.pool,
        sessions::NewSession {
            task_id: &task.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    // Left idle with no end reason, as a crash leaves a live session.
    sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-old")
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET status = 'pausing', paused_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workflow_state SET stage_entered_at = ? WHERE task_id = ?")
        .bind(Utc::now() + chrono::Duration::hours(1))
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let next = q.restarted_engine();
    Q::sweep(&next).await;
    assert_eq!(q.status(&task.id).await, "paused");
    let after = sessions::get(&q.pool, &old.id).await.unwrap().unwrap();
    assert_ne!(
        after.end_reason,
        Some(SessionEndReason::DaemonStopped),
        "an earlier entry's session was not cut off by the crash"
    );
}

#[tokio::test]
async fn a_paused_end_whose_task_cannot_be_read_marks_the_task_stuck() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    let old = q.sessions(&a.id).await.remove(0);
    // The task row can no longer be decoded, but can still be written.
    sqlx::query(
        "UPDATE tasks SET status = 'open', paused_at = NULL, created_at = 'garbage' WHERE id = ?",
    )
    .bind(&a.id)
    .execute(&q.pool)
    .await
    .unwrap();
    let def = Arc::new(WorkflowDefinition::parse(super::queue::FLOW, &q.dir).unwrap());
    q.engine
        .handle_requested_end(&a.id, &def, "coding", &old.id, None, None)
        .await;
    let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
        .bind(&a.id)
        .fetch_one(&q.pool)
        .await
        .unwrap();
    assert_eq!(status, "stuck");
    let reason: String = sqlx::query_scalar("SELECT stuck_reason FROM tasks WHERE id = ?")
        .bind(&a.id)
        .fetch_one(&q.pool)
        .await
        .unwrap();
    assert!(
        reason.contains("could not read the task after a paused turn ended"),
        "{reason}"
    );
}

/// The worktree of the group task `g` made by `Q::create_group`.
fn group_worktree(q: &Q, g: &Task) -> PathBuf {
    crate::worktree::worktree_path(&q.dir.join("repo"), "demo", &g.id).unwrap()
}

#[tokio::test]
async fn a_read_only_violation_found_by_the_sweep_makes_a_pausing_task_stuck() {
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    q.engine.pause_task(&g.id, false).await.unwrap();
    assert_eq!(q.status(&g.id).await, "pausing");
    // A read-only branch changed the worktree before the crash.
    fs::write(group_worktree(&q, &g).join("changed.txt"), "x").unwrap();
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&g.id).await, "stuck");
    let reason = task_of(&q, &g.id).await.stuck_reason.unwrap();
    assert!(reason.contains("changed the worktree"), "{reason}");
    assert_paused_at_invariant(&q, &g.id).await;
}

// ---- test 7: a gate reply while paused -------------------------------------------

#[tokio::test]
async fn a_gate_reply_while_paused_is_recorded_and_the_next_stage_is_held() {
    let q = Q::new(None).await;
    let a = create_flow(&q, GATE_FLOW, "pgate.yaml", "gate").await;
    let outcome = q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(outcome.status, "paused");
    q.engine
        .send_message_or_resume(&a.id, "go ahead")
        .await
        .unwrap();
    assert_eq!(q.status(&a.id).await, "paused");
    assert_eq!(
        current_stage(&q, &a.id).await,
        "work",
        "the transition is recorded"
    );
    assert!(
        q.queue_events(&a.id, EventType::ShellOutput)
            .await
            .is_empty()
    );
    let held_events = q.queue_events(&a.id, EventType::StageHeld).await;
    assert_eq!(held_events[0]["stage"], "work");
    assert_paused_at_invariant(&q, &a.id).await;
    // Only `resume` starts it.
    q.engine.resume_task(&a.id).await.unwrap();
    q.wait_status(&a.id, "closed").await;
}

#[tokio::test]
async fn a_gate_reply_that_reaches_the_terminal_stage_closes_a_paused_task() {
    let q = Q::new(None).await;
    let a = create_flow(&q, GATE_END_FLOW, "pgateend.yaml", "gate").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    q.engine
        .send_message_or_resume(&a.id, "done")
        .await
        .unwrap();
    q.wait_status(&a.id, "closed").await;
    assert!(task_of(&q, &a.id).await.paused_at.is_none());
}

// ---- test 8: 409s -------------------------------------------------------------------

/// A task at `coding` in `status`, with `paused_at` set when it should be.
async fn task_in(q: &Q, status: &str) -> Task {
    let task = q.task_at("coding", "open").await;
    match status {
        "open" => {}
        "pausing" | "paused" => {
            assert!(
                tasks::mark_pausing(&q.pool, &task.id, Utc::now())
                    .await
                    .unwrap()
            );
            if status == "paused" {
                assert!(tasks::mark_paused(&q.pool, &task.id).await.unwrap());
            }
        }
        other => {
            tasks::update_status(&q.pool, &task.id, other)
                .await
                .unwrap();
        }
    }
    task_of(q, &task.id).await
}

#[tokio::test]
async fn send_and_retry_are_refused_on_a_pausing_or_paused_task_and_name_resume() {
    let q = Q::new(None).await;
    for status in ["pausing", "paused"] {
        let t = task_in(&q, status).await;
        let err = q.engine.send_message(&t.id, "hello").await.unwrap_err();
        assert!(
            matches!(&err, SendMessageError::TaskPaused { status: s, .. } if s == status),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains(&format!("choco task resume {}", t.id)),
            "{err}"
        );
        let err = q
            .engine
            .retry_task(&t.id, RetryMode::Auto)
            .await
            .unwrap_err();
        assert!(matches!(err, RetryTaskError::Paused { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains(&format!("choco task resume {}", t.id)),
            "{err}"
        );
        // Unchanged, and nothing started.
        assert_eq!(q.status(&t.id).await, status);
        assert_paused_at_invariant(&q, &t.id).await;
        assert!(q.sessions(&t.id).await.is_empty());
        assert!(q.rows().await.is_empty());
        assert_eq!(q.engine.running_total(), 0);
    }
}

#[tokio::test]
async fn pause_and_resume_refusals_name_the_status_and_change_nothing() {
    let q = Q::new(None).await;
    let cases: [(&str, bool, &str); 6] = [
        ("stuck", false, "stuck tasks don't run"),
        ("cancelled", false, "the task is cancelled"),
        ("closed", false, "the task is closed"),
        ("paused", false, "already paused"),
        ("paused", true, "already paused"),
        ("pausing", false, "use --now"),
    ];
    for (status, now, words) in cases {
        let t = task_in(&q, status).await;
        let err = q.engine.pause_task(&t.id, now).await.unwrap_err();
        assert!(
            matches!(err, PauseTaskError::Refused(_)),
            "{status}: {err:?}"
        );
        assert!(err.to_string().contains(words), "{status}: {err}");
        assert_eq!(q.status(&t.id).await, status);
    }
    let stuck = task_in(&q, "stuck").await;
    let err = q.engine.pause_task(&stuck.id, false).await.unwrap_err();
    assert!(err.to_string().contains("choco task retry"), "{err}");
    for (status, words) in [
        ("open", "not paused"),
        ("stuck", "choco task retry"),
        ("closed", "the task is closed"),
        ("cancelled", "the task is cancelled"),
    ] {
        let t = task_in(&q, status).await;
        let err = q.engine.resume_task(&t.id).await.unwrap_err();
        assert!(
            matches!(err, ResumeTaskError::Refused(_)),
            "{status}: {err:?}"
        );
        assert!(err.to_string().contains(words), "{status}: {err}");
        assert_eq!(q.status(&t.id).await, status);
        assert!(q.sessions(&t.id).await.is_empty());
    }
    assert!(matches!(
        q.engine.pause_task("no-such", false).await,
        Err(PauseTaskError::NoSuchTask)
    ));
    assert!(matches!(
        q.engine.resume_task("no-such").await,
        Err(ResumeTaskError::NoSuchTask)
    ));
}

// ---- test 9: groups -------------------------------------------------------------------

async fn slot_state(q: &Q, id: &str, branch: &str) -> String {
    let state = workflow_state::get(&q.pool, id).await.unwrap().unwrap();
    state.payload["parallel"]["branches"][branch]["state"]
        .as_str()
        .unwrap_or("?")
        .to_string()
}

async fn wait_slot(q: &Q, id: &str, branch: &str, expected: &str) {
    crate::test_support::wait_until(&format!("{branch} to be {expected}"), || async {
        let state = slot_state(q, id, branch).await;
        if state == expected {
            Ok(())
        } else {
            Err(state)
        }
    })
    .await
}

#[tokio::test]
async fn a_group_pauses_as_a_whole_and_stays_pausing_until_the_last_branch_ends() {
    let q = Q::new(Some(2)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    let queued_row = q.wait_rows(1).await.remove(0).id;
    let outcome = q.engine.pause_task(&g.id, false).await.unwrap();
    assert_eq!((outcome.status.as_str(), outcome.running), ("pausing", 2));
    q.release("one");
    wait_slot(&q, &g.id, "one", "done").await;
    // One branch ended; the other still runs.
    assert_eq!(q.status(&g.id).await, "pausing");
    q.release("two");
    q.wait_status(&g.id, "paused").await;
    // Both ends were recorded; the queued branch did not start.
    assert_eq!(slot_state(&q, &g.id, "two").await, "done");
    q.assert_not_started("three").await;
    let rows = q.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, queued_row, "the row was not withdrawn");
    // Resume claims nothing for a branch that has a row; the dispatcher
    // starts it from its old place.
    let outcome = q.engine.resume_task(&g.id).await.unwrap();
    assert!(outcome.started.is_empty(), "{outcome:?}");
    q.wait_started("three").await;
    q.release("three");
    q.wait_status(&g.id, "closed").await;
}

#[tokio::test]
async fn pause_now_then_resume_restarts_only_the_unsettled_branches_resuming_their_sessions() {
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    q.release("one");
    wait_slot(&q, &g.id, "one", "done").await;
    q.engine.pause_task(&g.id, true).await.unwrap();
    q.wait_status(&g.id, "paused").await;
    // Their slots stay unsettled; their sessions ended `paused`.
    assert_eq!(slot_state(&q, &g.id, "two").await, "running");
    assert_eq!(slot_state(&q, &g.id, "three").await, "running");
    let all = q.sessions(&g.id).await;
    let of = |stage: &str| all.iter().find(|s| s.stage == stage).unwrap().clone();
    assert_eq!(of("two").end_reason, Some(SessionEndReason::Paused));
    assert_eq!(of("one").end_reason, None);
    for stage in ["two", "three"] {
        if of(stage).adapter_session_id.is_none() {
            sessions::set_adapter_session_id(&q.pool, &of(stage).id, &format!("adapter-{stage}"))
                .await
                .unwrap();
        }
    }
    plain_script(&q, "rest");
    q.release("rest");
    let outcome = q.engine.resume_task(&g.id).await.unwrap();
    let mut started = outcome.started.clone();
    started.sort();
    assert_eq!(started, vec!["three".to_string(), "two".to_string()]);
    let mut resumed = outcome.resumed.clone();
    resumed.sort();
    assert_eq!(resumed, started, "both continued their own sessions");
    q.wait_status(&g.id, "closed").await;
    let all = q.sessions(&g.id).await;
    for stage in ["two", "three"] {
        let old = of(stage);
        let new = all
            .iter()
            .find(|s| s.stage == stage && s.id != old.id)
            .unwrap();
        assert_eq!(
            new.resumed_from.as_deref(),
            Some(old.id.as_str()),
            "{stage}"
        );
    }
    assert_eq!(
        all.iter().filter(|s| s.stage == "one").count(),
        1,
        "one was not restarted"
    );
}

// ---- test 10: a paused task's queued rows ------------------------------------------------

#[tokio::test]
async fn a_paused_tasks_queued_row_waits_in_place_does_not_block_its_pool_and_starts_first_on_resume()
 {
    let q = Q::new(Some(1)).await;
    q.script(&[
        ("job-a", Ending::Report),
        ("job-b", Ending::Report),
        ("job-c", Ending::Report),
        ("job-d", Ending::Report),
    ]);
    let _a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    let b_row = q.wait_rows(1).await.remove(0).id;
    let outcome = q.engine.pause_task(&b.id, false).await.unwrap();
    assert_eq!(outcome.status, "paused", "a queued turn is not running");
    let c = q.create("job-c").await;
    // The paused task's row is skipped, so another task's turn gets the slot.
    q.release("job-a");
    q.wait_started("job-c").await;
    q.assert_not_started("job-b").await;
    let rows = q.rows().await;
    assert!(rows.iter().any(|r| r.id == b_row), "not withdrawn");
    assert!(
        q.queue_events(&b.id, EventType::TurnWithdrawn)
            .await
            .is_empty()
    );
    // Resume deletes nothing and claims nothing; the row is admissible again
    // with its old id, ahead of a later arrival.
    let outcome = q.engine.resume_task(&b.id).await.unwrap();
    assert!(outcome.started.is_empty(), "{outcome:?}");
    let _d = q.create("job-d").await;
    assert!(q.rows().await.iter().any(|r| r.id == b_row));
    q.release("job-c");
    q.wait_started("job-b").await;
    q.assert_not_started("job-d").await;
    let _ = c;
}

// ---- test 11: in_flight -------------------------------------------------------------------

#[tokio::test]
async fn in_flight_lists_a_pausing_task_with_running_work_and_not_a_paused_one() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    let listed = q.engine.in_flight().await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        (listed[0].task_id.as_str(), listed[0].kind.as_str()),
        (a.id.as_str(), "agent_turn")
    );

    // A pausing task whose current stage is held and has nothing running.
    let held_task = task_in(&q, "pausing").await;
    // A paused task.
    let paused = task_in(&q, "paused").await;
    let listed = q.engine.in_flight().await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|f| f.task_id.as_str()).collect();
    assert_eq!(ids, vec![a.id.as_str()]);
    assert!(!ids.contains(&held_task.id.as_str()) && !ids.contains(&paused.id.as_str()));
    // Once its turn ends the task is paused and no longer listed.
    q.release("job-a");
    q.wait_status(&a.id, "paused").await;
    assert!(q.engine.in_flight().await.unwrap().is_empty());
}

// ---- test 12: the move to paused happens once, after the last end --------------------------

#[tokio::test]
async fn many_ends_in_any_order_move_the_task_to_paused_exactly_once_after_the_last() {
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = create_group_flow(&q, &three_group_flow(), "agroup3.yaml", "fix-job").await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    q.engine.pause_task(&g.id, false).await.unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (engine, pool, stop, id) = (
            Arc::clone(&q.engine),
            q.pool.clone(),
            Arc::clone(&stop),
            g.id.clone(),
        );
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let status = tasks::get(&pool, &id).await.unwrap().unwrap().status;
                if status == "paused" {
                    // Never paused while a turn of the task runs or is
                    // still applying its result.
                    assert_eq!(engine.turn_gate().busy_for_task(&id), 0);
                }
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
        })
    };
    for name in ["three", "one", "two"] {
        q.release(name);
    }
    q.wait_status(&g.id, "paused").await;
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    sampler.await.unwrap();
    // The next stage is held: entered, with no session and no queue row.
    assert_eq!(current_stage(&q, &g.id).await, "fix");
    assert!(q.sessions(&g.id).await.iter().all(|s| s.stage != "fix"));
    assert!(q.rows().await.is_empty());
    let events = q.events(&g.id).await;
    let paused: Vec<usize> = (0..events.len())
        .filter(|i| events[*i].event_type == EventType::TaskPaused)
        .collect();
    assert_eq!(paused.len(), 1, "exactly one TaskPaused");
    let last_branch = (0..events.len())
        .filter(|i| events[*i].event_type == EventType::BranchFinished)
        .max()
        .unwrap();
    assert!(
        last_branch < paused[0],
        "paused after the last branch ended"
    );
}

// ---- test 13: a turn that fails while pausing --------------------------------------------------

#[tokio::test]
async fn a_turn_that_fails_while_pausing_makes_the_task_stuck_and_clears_paused_at() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Crash)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    q.release("job-a");
    q.wait_status(&a.id, "stuck").await;
    assert_paused_at_invariant(&q, &a.id).await;
    assert!(task_of(&q, &a.id).await.stuck_reason.is_some());
}

#[tokio::test]
async fn a_failure_reaches_stuck_from_pausing_and_from_paused_in_one_write() {
    let q = Q::new(None).await;
    for status in ["pausing", "paused"] {
        let t = task_in(&q, status).await;
        assert!(t.paused_at.is_some());
        assert!(
            tasks::mark_stuck(&q.pool, &t.id, "boom").await.unwrap(),
            "{status}"
        );
        let after = task_of(&q, &t.id).await;
        assert_eq!(after.status, "stuck");
        assert_eq!(after.stuck_reason.as_deref(), Some("boom"));
        assert!(after.paused_at.is_none(), "{status}");
    }
    // The compare-and-set still refuses a closed or cancelled task.
    for status in ["closed", "cancelled"] {
        let t = task_in(&q, status).await;
        assert!(!tasks::mark_stuck(&q.pool, &t.id, "boom").await.unwrap());
        assert_eq!(q.status(&t.id).await, status);
    }
}

// ---- test 14 / 16: paused_at and the rows that stay as they are --------------------------------

#[tokio::test]
async fn every_writer_that_moves_a_task_out_of_a_pause_clears_paused_at() {
    let q = Q::new(None).await;
    for status in ["pausing", "paused"] {
        // closed (a finished turn or gate reply reaching the terminal stage)
        let t = task_in(&q, status).await;
        tasks::update_status(&q.pool, &t.id, "closed")
            .await
            .unwrap();
        assert_paused_at_invariant(&q, &t.id).await;
        // cancelled works from both
        let t = task_in(&q, status).await;
        q.engine.cancel_task(&t.id, false).await.unwrap();
        assert_eq!(q.status(&t.id).await, "cancelled");
        assert_paused_at_invariant(&q, &t.id).await;
        // resumed
        let t = task_in(&q, status).await;
        let mut conn = q.pool.acquire().await.unwrap();
        assert!(tasks::resume_in(&mut conn, &t.id).await.unwrap());
        drop(conn);
        assert_eq!(q.status(&t.id).await, "open");
        assert_paused_at_invariant(&q, &t.id).await;
    }
}

#[tokio::test]
async fn the_rows_that_do_not_change_are_proven_unchanged_for_the_new_statuses() {
    let q = Q::new(None).await;
    let open = task_in(&q, "open").await;
    let pausing = task_in(&q, "pausing").await;
    let paused = task_in(&q, "paused").await;
    // `waiting_on_human` stays false for a paused task at a gate.
    let gate = q.task_at("gate", "open").await;
    sqlx::query("UPDATE workflow_state SET stage_kind = 'human_gate' WHERE task_id = ?")
        .bind(&gate.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let rows = tasks::list_summaries(&q.pool, None, &[], tasks::SummaryOrder::Id, None)
        .await
        .unwrap();
    let waiting = |id: &str| {
        rows.iter()
            .find(|r| r.task.id == id)
            .unwrap()
            .waiting_on_human
    };
    assert!(waiting(&gate.id));
    tasks::mark_pausing(&q.pool, &gate.id, Utc::now())
        .await
        .unwrap();
    tasks::mark_paused(&q.pool, &gate.id).await.unwrap();
    let rows = tasks::list_summaries(&q.pool, None, &[], tasks::SummaryOrder::Id, None)
        .await
        .unwrap();
    assert!(
        !rows
            .iter()
            .find(|r| r.task.id == gate.id)
            .unwrap()
            .waiting_on_human
    );
    // `?status=pausing,paused` filters work as they are.
    let both = tasks::list_summaries(
        &q.pool,
        None,
        &["pausing".to_string(), "paused".to_string()],
        tasks::SummaryOrder::Id,
        None,
    )
    .await
    .unwrap();
    assert_eq!(both.len(), 3);
    let _ = (open, waiting(&pausing.id), waiting(&paused.id));
    // `count_by_status` shows the new values.
    let counts = tasks::count_by_status(&q.pool).await.unwrap();
    assert_eq!(counts.get("pausing"), Some(&1));
    assert_eq!(counts.get("paused"), Some(&2));
    // Rows 10 and 11 count pausing and paused tasks too.
    let prefix = "/w/";
    for t in [&pausing, &paused] {
        sqlx::query("UPDATE tasks SET workflow_path = '/w/x.yaml' WHERE id = ?")
            .bind(&t.id)
            .execute(&q.pool)
            .await
            .unwrap();
    }
    assert_eq!(
        tasks::count_active_with_workflow_path_prefix(&q.pool, prefix)
            .await
            .unwrap(),
        2
    );
    sqlx::query("UPDATE tasks SET workflow_path = NULL WHERE id IN (?, ?)")
        .bind(&pausing.id)
        .bind(&paused.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let defs = tasks::active_workflow_defs_without_path(&q.pool)
        .await
        .unwrap();
    assert!(defs.len() >= 2, "{defs:?}");
}

// ---- test 18: the watcher re-enters a stage a resume left to it ---------------------------------

#[tokio::test]
async fn a_paused_end_on_an_open_task_re_enters_the_stage_resuming_the_session() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    let old = q.sessions(&a.id).await.remove(0);
    if old.adapter_session_id.is_none() {
        sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
            .await
            .unwrap();
    }
    // A resume landed while the killed session was still draining: the task
    // is open and the watcher sees the paused end.
    sqlx::query("UPDATE tasks SET status = 'open', paused_at = NULL WHERE id = ?")
        .bind(&a.id)
        .execute(&q.pool)
        .await
        .unwrap();
    plain_script(&q, "job-a");
    q.release("job-a");
    let def = Arc::new(WorkflowDefinition::parse(super::queue::FLOW, &q.dir).unwrap());
    q.engine
        .handle_requested_end(&a.id, &def, "coding", &old.id, None, None)
        .await;
    q.wait_status(&a.id, "closed").await;
    let all = q.sessions(&a.id).await;
    assert_eq!(all.len(), 2);
    assert_eq!(
        all.iter()
            .find(|s| s.id != old.id)
            .unwrap()
            .resumed_from
            .as_deref(),
        Some(old.id.as_str())
    );
    // Idempotent: the session is no longer the stage's current one.
    q.engine
        .handle_requested_end(&a.id, &def, "coding", &old.id, None, None)
        .await;
    assert_eq!(q.sessions(&a.id).await.len(), 2);
}

// ---- test 19: the dispatcher re-checks under the task lock ---------------------------------------

#[tokio::test]
async fn a_pause_between_the_unlocked_pick_and_the_locked_one_starts_nothing() {
    let q = Q::build(Some(1), super::queue::queue_timers(), false).await;
    q.script(&[("job-r", Ending::Report)]);
    let task = q.task_at("coding", "open").await;
    q.insert_row(&task.id, "coding").await;
    // Hold the task's lock so the pass stops between its two picks.
    let lock = q.engine.lock_for_task(&task.id).await;
    let guard = lock.lock().await;
    let engine = Arc::clone(&q.engine);
    let pass = tokio::spawn(async move { engine.dispatch_pass().await });
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert!(
        tasks::mark_pausing(&q.pool, &task.id, Utc::now())
            .await
            .unwrap()
    );
    drop(guard);
    assert_eq!(pass.await.unwrap(), crate::engine::turns::Pass::Progress);
    q.assert_not_started("job-r").await;
    assert_eq!(q.rows().await.len(), 1, "the row stays");
    assert!(q.sessions(&task.id).await.is_empty());
    assert_eq!(q.engine.running_total(), 0);
}

// ---- test 20: a group that settles with failures -------------------------------------------------

#[tokio::test]
async fn a_group_settling_with_failures_marks_a_pausing_or_paused_task_stuck() {
    let q = Q::new(None).await;
    for status in ["pausing", "paused"] {
        let t = task_in(&q, status).await;
        let state = workflow_state::get(&q.pool, &t.id).await.unwrap().unwrap();
        let settled = workflow_state::settle_with_failures(
            &q.pool,
            &t.id,
            workflow_state::WorkflowStateUpdate {
                current_stage: state.current_stage,
                stage_kind: "parallel".to_string(),
                loop_counters: state.loop_counters,
                payload: json!({"settled": true}),
                enters_stage: false,
            },
            "a branch failed",
        )
        .await
        .unwrap()
        .unwrap();
        assert!(settled.marked_stuck, "{status}");
        assert_eq!(q.status(&t.id).await, "stuck");
        assert_paused_at_invariant(&q, &t.id).await;
    }
}

// ---- test 22: failure paths ---------------------------------------------------------------------------

#[tokio::test]
async fn a_settle_pause_write_that_fails_logs_at_error_leaves_the_task_pausing_and_resume_repairs_it()
 {
    let logs = super::queue::LogBuf::default();
    let _logging = logs.capture();
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_paused BEFORE UPDATE ON tasks WHEN NEW.status = 'paused' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    q.release("job-a");
    q.wait_running(0).await;
    tokio::time::sleep(StdDuration::from_millis(500)).await;
    assert_eq!(q.status(&a.id).await, "pausing", "the safe side");
    let text = logs.text();
    assert!(
        text.contains("ERROR") && text.contains("could not mark a pausing task paused"),
        "{text}"
    );
    // A later resume repairs it: the held stage starts.
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert_eq!(outcome.started, vec!["checks".to_string()]);
    q.wait_status(&a.id, "closed").await;
}

#[tokio::test]
async fn a_failed_stop_is_logged_at_error_and_listed_in_the_event_without_failing_the_pause() {
    let logs = super::queue::LogBuf::default();
    let _logging = logs.capture();
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    // The workflow file is gone, so the turns to stop cannot be found.
    fs::remove_file(&q.flow).unwrap();
    let outcome = q.engine.pause_task(&a.id, true).await.unwrap();
    assert!(!outcome.stop_failures.is_empty());
    assert!(matches!(
        q.status(&a.id).await.as_str(),
        "pausing" | "paused"
    ));
    let event = q
        .queue_events(&a.id, EventType::TaskPausing)
        .await
        .remove(0);
    assert_eq!(
        event["stop_failures"].as_array().unwrap().len(),
        outcome.stop_failures.len()
    );
    let text = logs.text();
    assert!(
        text.contains("ERROR") && text.contains("could not find the turns to stop"),
        "{text}"
    );
    q.release("job-a");
}

#[tokio::test]
async fn a_resume_whose_start_fails_marks_the_task_stuck() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    // The role's CLI is now one this daemon does not know.
    sqlx::query("UPDATE tasks SET config = ? WHERE id = ?")
        .bind(json!({ "roles": { "coder": { "cli": "no-such-cli" } } }).to_string())
        .bind(&a.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let err = q.engine.resume_task(&a.id).await.unwrap_err();
    assert!(matches!(err, ResumeTaskError::Enter(_)), "{err:?}");
    assert_eq!(q.status(&a.id).await, "stuck");
    let reason = task_of(&q, &a.id).await.stuck_reason.unwrap();
    assert!(reason.contains("resume failed"), "{reason}");
    assert_paused_at_invariant(&q, &a.id).await;
}

#[tokio::test]
async fn a_resume_whose_start_fails_and_cannot_mark_stuck_says_so() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    sqlx::query("UPDATE tasks SET config = ? WHERE id = ?")
        .bind(json!({ "roles": { "coder": { "cli": "no-such-cli" } } }).to_string())
        .bind(&a.id)
        .execute(&q.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_stuck BEFORE UPDATE ON tasks WHEN NEW.status = 'stuck' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    let err = q.engine.resume_task(&a.id).await.unwrap_err();
    assert!(
        matches!(err, ResumeTaskError::StartFailedNotStuck(_)),
        "{err:?}"
    );
    assert!(
        err.to_string().contains("marking the task stuck failed"),
        "{err}"
    );
}

#[tokio::test]
async fn a_pause_write_that_fails_is_an_error_and_stops_nothing() {
    let q = Q::new(None).await;
    let a = create_flow(&q, POLL_FLOW, "ppoll.yaml", "poll").await;
    sqlx::query(
        "CREATE TRIGGER refuse_pausing BEFORE UPDATE ON tasks WHEN NEW.status = 'pausing' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    let err = q.engine.pause_task(&a.id, false).await.unwrap_err();
    assert!(matches!(err, PauseTaskError::Db(_)), "{err:?}");
    assert_eq!(q.status(&a.id).await, "open");
    assert!(q.engine.has_detached_runner(&a.id), "nothing was stopped");
    q.engine.cancel_task(&a.id, false).await.unwrap();
}

// ---- a crash while pausing must not turn a failed turn into `paused` --------------------------------

/// Marks a session as it was left by a failed turn: exited with `no_report`.
async fn fail_session(q: &Q, session_id: &str) {
    sqlx::query("UPDATE sessions SET status = 'exited', end_reason = 'no_report' WHERE id = ?")
        .bind(session_id)
        .execute(&q.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_pausing_task_whose_turn_failed_before_a_crash_is_stuck_after_the_sweep_not_paused() {
    let q = Q::new(None).await;
    let task = q.task_at("coding", "open").await;
    let session = sessions::create(
        &q.pool,
        sessions::NewSession {
            task_id: &task.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    fail_session(&q, &session.id).await;
    sqlx::query("UPDATE tasks SET status = 'pausing', paused_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let next = q.restarted_engine();
    Q::sweep(&next).await;
    assert_eq!(q.status(&task.id).await, "stuck");
    let reason = task_of(&q, &task.id).await.stuck_reason.unwrap();
    assert!(
        reason.contains("turn of stage 'coding' failed") && reason.contains("no_report"),
        "{reason}"
    );
    assert_paused_at_invariant(&q, &task.id).await;
}

#[tokio::test]
async fn a_pausing_group_with_one_failed_branch_records_it_failed_and_is_not_paused_after_the_sweep()
 {
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    q.engine.pause_task(&g.id, false).await.unwrap();
    assert_eq!(q.status(&g.id).await, "pausing");
    let all = q.sessions(&g.id).await;
    let of = |stage: &str| all.iter().find(|s| s.stage == stage).unwrap().clone();
    fail_session(&q, &of("one").id).await;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(slot_state(&q, &g.id, "one").await, "failed");
    let after = |stage: &str| {
        let id = of(stage).id;
        let pool = q.pool.clone();
        async move { sessions::get(&pool, &id).await.unwrap().unwrap() }
    };
    assert_eq!(
        after("two").await.end_reason,
        Some(SessionEndReason::DaemonStopped)
    );
    assert_eq!(
        after("three").await.end_reason,
        Some(SessionEndReason::DaemonStopped)
    );
    assert_eq!(q.status(&g.id).await, "stuck");
    let reason = task_of(&q, &g.id).await.stuck_reason.unwrap();
    assert!(reason.contains("branch 'one'"), "{reason}");
    assert_paused_at_invariant(&q, &g.id).await;
}

#[tokio::test]
async fn resume_marks_a_task_stuck_rather_than_reopening_a_stage_whose_run_failed() {
    let q = Q::new(None).await;
    let task = q.task_at("coding", "open").await;
    let session = sessions::create(
        &q.pool,
        sessions::NewSession {
            task_id: &task.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    fail_session(&q, &session.id).await;
    sqlx::query("UPDATE tasks SET status = 'paused', paused_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let err = q.engine.resume_task(&task.id).await.unwrap_err();
    assert!(err.to_string().contains("retry"), "{err}");
    assert_eq!(q.status(&task.id).await, "stuck");
    assert_paused_at_invariant(&q, &task.id).await;
    assert_eq!(q.sessions(&task.id).await.len(), 1, "nothing started");
}

#[tokio::test]
async fn resume_leaves_a_failed_branch_to_its_watcher_so_siblings_still_report() {
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::UsageLimit),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    q.engine.pause_task(&g.id, false).await.unwrap();
    // `resume` holds the task lock while branch `one` has ended as a failure
    // that its watcher has not yet recorded.
    let lock = q.engine.lock_for_task(&g.id).await;
    let guard = lock.lock().await;
    q.release("one");
    crate::test_support::wait_until("branch one's session to end", || async {
        let all = q.sessions(&g.id).await;
        match all.iter().find(|s| s.stage == "one") {
            Some(s) if s.end_reason.is_some() => Ok(()),
            _ => Err("running".to_string()),
        }
    })
    .await;
    let result = q.engine.resume_task_locked(&g.id).await;
    drop(guard);
    assert!(result.is_ok(), "{result:?}");
    assert_ne!(
        q.status(&g.id).await,
        "stuck",
        "resume pre-empted the watcher"
    );
    q.release("two");
    q.release("three");
    wait_slot(&q, &g.id, "two", "done").await;
    wait_slot(&q, &g.id, "three", "done").await;
    wait_slot(&q, &g.id, "one", "failed").await;
    q.wait_status(&g.id, "stuck").await;
    assert_paused_at_invariant(&q, &g.id).await;
}

#[tokio::test]
async fn a_resume_that_cannot_mark_the_task_stuck_says_so_and_keeps_the_status() {
    let q = Q::new(None).await;
    let task = q.task_at("coding", "open").await;
    let session = sessions::create(
        &q.pool,
        sessions::NewSession {
            task_id: &task.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    fail_session(&q, &session.id).await;
    sqlx::query("UPDATE tasks SET status = 'paused', paused_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_stuck BEFORE UPDATE ON tasks WHEN NEW.status = 'stuck' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    let err = q.engine.resume_task(&task.id).await.unwrap_err();
    assert!(
        matches!(err, ResumeTaskError::StuckWriteFailed(_)),
        "{err:?}"
    );
    assert!(!err.to_string().contains("now stuck"), "{err}");
    assert_eq!(q.status(&task.id).await, "paused");
}

// ---- the watcher's restart and `resume` decide under one lock -------------------------------------------

#[tokio::test]
async fn a_watcher_restart_waits_for_the_task_lock_so_resume_and_the_watcher_start_one_turn() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, true).await.unwrap();
    q.wait_status(&a.id, "paused").await;
    let old = q.sessions(&a.id).await.remove(0);
    if old.adapter_session_id.is_none() {
        sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
            .await
            .unwrap();
    }
    plain_script(&q, "job-a");
    let def = Arc::new(WorkflowDefinition::parse(super::queue::FLOW, &q.dir).unwrap());

    // `resume` holds the lock and has written `open`, but not started yet.
    let lock = q.engine.lock_for_task(&a.id).await;
    let guard = lock.lock().await;
    sqlx::query("UPDATE tasks SET status = 'open', paused_at = NULL WHERE id = ?")
        .bind(&a.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let engine = Arc::clone(&q.engine);
    let (task_id, session_id, watcher_def) = (a.id.clone(), old.id.clone(), Arc::clone(&def));
    let watcher = tokio::spawn(async move {
        engine
            .reenter_after_pause(&task_id, &watcher_def, "coding", &session_id, None)
            .await
    });
    tokio::time::sleep(StdDuration::from_millis(400)).await;
    assert_eq!(
        q.sessions(&a.id).await.len(),
        1,
        "the watcher started a turn without the task lock"
    );

    // `resume` starts the stage, then releases the lock.
    let state = workflow_state::get(&q.pool, &a.id).await.unwrap().unwrap();
    q.engine
        .reenter_stage_locked(&a.id, &def, "coding", &state.payload, "resume", None)
        .await
        .unwrap();
    drop(guard);
    watcher.await.unwrap();
    assert_eq!(q.sessions(&a.id).await.len(), 2, "exactly one new session");
    assert_eq!(q.engine.running_total(), 1, "exactly one running turn");
    q.release("job-a");
    q.wait_status(&a.id, "closed").await;
}

// ---- resume starts a held group and a held watching gate -------------------------------------------------

#[tokio::test]
async fn resume_starts_a_group_that_was_entered_while_held_with_every_branch_once() {
    let q = Q::new(None).await;
    let yaml = "name: pcode\nworktree: true\nroles:\n  coder:\n    cli: claude\n    model: sonnet\n  reviewer:\n    cli: claude\n    model: sonnet\n    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: { done: panel }\n  panel:\n    kind: parallel\n    branches:\n      one: { kind: agent_turn, role: reviewer, prompt_file: one.md }\n      two: { kind: agent_turn, role: reviewer, prompt_file: two.md }\n      three: { kind: agent_turn, role: reviewer, prompt_file: three.md }\n    on: { done: finished }\n  finished:\n    kind: terminal\n";
    let path = q.dir.join("pcode.yaml");
    fs::write(&path, yaml).unwrap();
    q.script_branches(&[
        ("JOB-CODE", "code", Ending::Report),
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let repo = q.dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    super::init_git_repo(&repo).await;
    let a = q
        .engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(path),
            "code",
            "JOB-CODE",
            json!({ "cwd": repo.to_string_lossy() }),
            None,
            &[],
        )
        .await
        .unwrap();
    q.wait_started("code").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    q.release("code");
    q.wait_status(&a.id, "paused").await;
    assert_eq!(current_stage(&q, &a.id).await, "panel");
    for name in ["one", "two", "three"] {
        q.assert_not_started(name).await;
    }
    assert_eq!(q.sessions(&a.id).await.len(), 1, "no branch session yet");

    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert_eq!(outcome.started, vec!["panel".to_string()]);
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
        q.release(name);
    }
    q.wait_status(&a.id, "closed").await;
    let all = q.sessions(&a.id).await;
    for branch in ["one", "two", "three"] {
        assert_eq!(
            all.iter().filter(|s| s.stage == branch).count(),
            1,
            "branch {branch} ran exactly once"
        );
    }
}

#[tokio::test]
async fn resume_restarts_a_held_gates_watcher_with_the_window_shifted_by_the_time_paused() {
    let q = Q::new(None).await;
    let yaml = "name: pgatew\nstages:\n  review:\n    kind: human_gate\n    on: { resumed: finished, timeout: finished }\n    watch:\n      command: \"echo WAITING\"\n      interval: 1s\n      timeout: 60s\n      outcomes:\n        - match: \"NEVER_MATCHES_XYZ\"\n          then: resumed\n  finished:\n    kind: terminal\n";
    let a = create_flow(&q, yaml, "pgatew.yaml", "gatew").await;
    assert!(q.engine.has_detached_runner(&a.id), "the gate is watched");
    let deadline = |state: &chocofactory_core::models::WorkflowState| {
        DateTime::parse_from_rfc3339(state.payload["poll_window"]["deadline"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc)
    };
    let before = workflow_state::get(&q.pool, &a.id).await.unwrap().unwrap();
    q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(q.status(&a.id).await, "paused");
    assert!(!q.engine.has_detached_runner(&a.id), "the watcher stopped");
    tokio::time::sleep(StdDuration::from_secs(2)).await;

    q.engine.resume_task(&a.id).await.unwrap();
    assert!(q.engine.has_detached_runner(&a.id), "watching again");
    let after = workflow_state::get(&q.pool, &a.id).await.unwrap().unwrap();
    let shifted = deadline(&after) - deadline(&before);
    assert!(
        shifted >= chrono::Duration::milliseconds(1500),
        "the deadline moved by {shifted}"
    );
    q.engine.cancel_task(&a.id, false).await.unwrap();
}

// ---- small fixes: the stop's read error is reported; send names resume ----------------------------------

#[tokio::test]
async fn a_pause_now_that_cannot_read_the_stage_reports_it_in_stop_failures() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    sqlx::query("UPDATE workflow_state SET payload = 'not json' WHERE task_id = ?")
        .bind(&a.id)
        .execute(&q.pool)
        .await
        .unwrap();
    let outcome = q.engine.pause_task(&a.id, true).await.unwrap();
    assert_eq!(outcome.stop_failures.len(), 1, "{outcome:?}");
    assert!(
        outcome.stop_failures[0].contains("no running turn was stopped"),
        "{outcome:?}"
    );
    assert_eq!(q.status(&a.id).await, "pausing");
}

#[tokio::test]
async fn send_at_a_single_shot_stage_of_a_paused_task_names_resume() {
    let q = Q::new(None).await;
    for status in ["pausing", "paused"] {
        let t = task_in(&q, status).await;
        let err = q
            .engine
            .send_message_or_resume(&t.id, "hello")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("choco task resume {}", t.id)),
            "{err}"
        );
        assert_eq!(q.status(&t.id).await, status);
    }
}

// ---- #304: a paused task is never left open with nothing running ---------------------------

use crate::engine::pause::run_failure;

fn group_flow(branches: &[&str]) -> String {
    let list: String = branches
        .iter()
        .map(|b| format!("      {b}: {{ kind: agent_turn, role: coder, prompt_file: {b}.md }}\n"))
        .collect();
    format!(
        "name: agroup\nworktree: true\nroles:\n  coder:\n    cli: claude\n    model: sonnet\n    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n  panel:\n    kind: parallel\n    branches:\n{list}    on: {{ done: fix }}\n  fix:\n    kind: agent_turn\n    role: coder\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
    )
}

fn three_group_flow() -> String {
    group_flow(&["one", "two", "three"])
}

/// Creates the task of a group flow the way `Q::create_group` does: in a git
/// repository, since the read-only branches snapshot a worktree.
async fn create_group_flow(q: &Q, yaml: &str, file: &str, name: &str) -> Task {
    let repo = q.dir.join("repo");
    if !repo.exists() {
        fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo).await;
    }
    let path = q.dir.join(file);
    fs::write(&path, yaml).unwrap();
    q.engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(path),
            name,
            name,
            json!({ "cwd": repo.to_string_lossy() }),
            None,
            &[],
        )
        .await
        .unwrap()
}

async fn new_session(q: &Q, task: &Task, stage: &str) -> Session {
    sessions::create(
        &q.pool,
        sessions::NewSession {
            task_id: &task.id,
            stage,
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap()
}

/// Sets a session's end as a crash and the sweep's recovery would leave it.
async fn set_end(q: &Q, session_id: &str, status: &str, reason: Option<&str>, adapter: bool) {
    sqlx::query("UPDATE sessions SET status = ?, end_reason = ? WHERE id = ?")
        .bind(status)
        .bind(reason)
        .bind(session_id)
        .execute(&q.pool)
        .await
        .unwrap();
    if adapter {
        sessions::set_adapter_session_id(&q.pool, session_id, &format!("adapter-{session_id}"))
            .await
            .unwrap();
    }
}

async fn force_paused(q: &Q, task_id: &str) {
    sqlx::query("UPDATE tasks SET status = 'paused', paused_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(task_id)
        .execute(&q.pool)
        .await
        .unwrap();
}

/// A `paused` task at `coding` whose current-entry session ended as given.
async fn paused_single(
    q: &Q,
    status: &str,
    reason: Option<&str>,
    adapter: bool,
) -> (Task, Session) {
    let task = q.task_at("coding", "open").await;
    let session = new_session(q, &task, "coding").await;
    set_end(q, &session.id, status, reason, adapter).await;
    force_paused(q, &task.id).await;
    (task, session)
}

/// A group paused with `pause --now` while the branches not in `done` ran:
/// their slots are `running` and their sessions ended `paused`.
async fn paused_group(q: &Q, done: &[&str]) -> Task {
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = create_group_flow(q, &three_group_flow(), "agroup3.yaml", "fix-job").await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    for name in done {
        q.release(name);
        wait_slot(q, &g.id, name, "done").await;
    }
    q.engine.pause_task(&g.id, true).await.unwrap();
    q.wait_status(&g.id, "paused").await;
    g
}

async fn session_of(q: &Q, task_id: &str, stage: &str) -> Session {
    q.sessions(task_id)
        .await
        .into_iter()
        .find(|s| s.stage == stage)
        .unwrap()
}

async fn reload(q: &Q, session_id: &str) -> Session {
    sessions::get(&q.pool, session_id).await.unwrap().unwrap()
}

async fn set_slot(q: &Q, task_id: &str, branch: &str, state: &str) {
    sqlx::query("UPDATE workflow_state SET payload = json_set(payload, ?, ?) WHERE task_id = ?")
        .bind(format!("$.parallel.branches.{branch}.state"))
        .bind(state)
        .bind(task_id)
        .execute(&q.pool)
        .await
        .unwrap();
}

fn refused_text(err: &ResumeTaskError) -> String {
    assert!(matches!(err, ResumeTaskError::Refused(_)), "{err:?}");
    err.to_string()
}

#[tokio::test]
async fn run_failure_answers_every_row_of_the_table() {
    let q = Q::new(None).await;
    let task = q.task_at("coding", "open").await;
    let base = new_session(&q, &task, "coding").await;
    let rows: &[(SessionStatus, Option<SessionEndReason>, bool)] = &[
        (SessionStatus::Active, None, false),
        (SessionStatus::Idle, None, false),
        (SessionStatus::Idle, Some(SessionEndReason::NoReport), false),
        (SessionStatus::Idle, Some(SessionEndReason::Reaped), true),
        (
            SessionStatus::Idle,
            Some(SessionEndReason::Cancelled),
            false,
        ),
        (
            SessionStatus::Idle,
            Some(SessionEndReason::DaemonStopped),
            false,
        ),
        (SessionStatus::Exited, None, true),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::NoReport),
            true,
        ),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::Lingered),
            true,
        ),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::StartFailed),
            true,
        ),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::Interrupted),
            true,
        ),
        (SessionStatus::Exited, Some(SessionEndReason::Reaped), true),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::Cancelled),
            false,
        ),
        (SessionStatus::Exited, Some(SessionEndReason::Paused), false),
        (
            SessionStatus::Exited,
            Some(SessionEndReason::DaemonStopped),
            false,
        ),
    ];
    for (status, reason, failed) in rows {
        let mut session = base.clone();
        session.status = *status;
        session.end_reason = *reason;
        let got = run_failure(&session);
        assert_eq!(got.is_some(), *failed, "{status:?}/{reason:?}: {got:?}");
        if let Some(text) = got {
            let want = reason.map_or("none".to_string(), |r| r.to_string());
            assert_eq!(
                text,
                format!("its session ended: {status}, end reason {want}")
            );
        }
    }
}

// (a)–(c) run on a restarted engine with an empty gate: the gate is not
// consulted after a restart, only the database.
#[tokio::test]
async fn a_paused_task_whose_turn_failed_before_a_crash_is_stuck_after_the_sweep() {
    let q = Q::new(None).await;
    let (task, _) = paused_single(&q, "exited", None, false).await;
    let next = q.restarted_engine();
    Q::sweep(&next).await;
    assert_eq!(q.status(&task.id).await, "stuck");
    let t = task_of(&q, &task.id).await;
    let reason = t.stuck_reason.unwrap();
    assert!(
        reason.contains("turn of stage 'coding' failed") && reason.contains("end reason none"),
        "{reason}"
    );
    assert!(t.paused_at.is_none());
    let err = next.resume_task(&task.id).await.unwrap_err();
    assert!(refused_text(&err).contains("retry"), "{err}");
    assert_eq!(q.sessions(&task.id).await.len(), 1);
}

#[tokio::test]
async fn resume_on_a_failed_run_with_no_sweep_marks_the_task_stuck_and_starts_nothing() {
    let q = Q::new(None).await;
    let (task, _) = paused_single(&q, "exited", None, false).await;
    let next = q.restarted_engine();
    let err = next.resume_task(&task.id).await.unwrap_err();
    assert!(refused_text(&err).contains("end reason none"), "{err}");
    let t = task_of(&q, &task.id).await;
    assert_eq!(t.status, "stuck");
    assert!(t.paused_at.is_none());
    assert!(t.stuck_reason.unwrap().contains("end reason none"));
    assert_eq!(q.sessions(&task.id).await.len(), 1);
    assert!(
        q.queue_events(&task.id, EventType::TaskResumed)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_paused_group_with_a_failed_branch_is_stuck_after_the_sweep_and_retry_restarts_only_it() {
    let q = Q::new(None).await;
    let g = paused_group(&q, &["two", "three"]).await;
    let one = session_of(&q, &g.id, "one").await;
    set_end(&q, &one.id, "exited", Some("no_report"), true).await;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(slot_state(&q, &g.id, "one").await, "failed");
    let finished = q.queue_events(&g.id, EventType::BranchFinished).await;
    assert!(
        finished
            .iter()
            .any(|e| e["branch"] == "one" && e["state"] == "failed"),
        "{finished:?}"
    );
    assert_eq!(q.status(&g.id).await, "stuck");
    let t = task_of(&q, &g.id).await;
    assert!(t.stuck_reason.unwrap().contains("branch 'one'"));
    assert!(t.paused_at.is_none());
    plain_script(&q, "one");
    q.release("one");
    next.retry_task(&g.id, RetryMode::Auto).await.unwrap();
    let all = q.sessions(&g.id).await;
    assert_eq!(all.iter().filter(|s| s.stage == "one").count(), 2);
    assert_eq!(all.iter().filter(|s| s.stage == "two").count(), 1);
    assert_eq!(all.iter().filter(|s| s.stage == "three").count(), 1);
}

#[tokio::test]
async fn a_mixed_paused_group_fails_the_failed_branch_and_records_the_completed_one() {
    let q = Q::new(None).await;
    let g = paused_group(&q, &["three"]).await;
    let one = session_of(&q, &g.id, "one").await;
    let two = session_of(&q, &g.id, "two").await;
    set_end(&q, &one.id, "exited", Some("no_report"), true).await;
    set_end(&q, &two.id, "idle", None, true).await;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(slot_state(&q, &g.id, "one").await, "failed");
    let two_after = reload(&q, &two.id).await;
    assert_eq!(two_after.status, SessionStatus::Exited);
    assert_eq!(two_after.end_reason, Some(SessionEndReason::DaemonStopped));
    assert_eq!(q.status(&g.id).await, "stuck");
    plain_script(&q, "one");
    q.release("one");
    next.retry_task(&g.id, RetryMode::Auto).await.unwrap();
    let all = q.sessions(&g.id).await;
    assert_eq!(all.iter().filter(|s| s.stage == "one").count(), 2);
    assert_eq!(all.iter().filter(|s| s.stage == "two").count(), 2);
    assert_eq!(all.iter().filter(|s| s.stage == "three").count(), 1);
}

#[tokio::test]
async fn a_paused_single_stage_with_a_completed_unapplied_turn_stays_paused_and_resume_reruns_its_session()
 {
    let q = Q::new(None).await;
    let (task, old) = paused_single(&q, "idle", None, true).await;
    let next = q.restarted_engine();
    Q::sweep(&next).await;
    assert_eq!(q.status(&task.id).await, "paused");
    let after = reload(&q, &old.id).await;
    assert_eq!(after.end_reason, Some(SessionEndReason::DaemonStopped));
    plain_script(&q, "job-c");
    let outcome = next.resume_task(&task.id).await.unwrap();
    assert_eq!(outcome.resumed, vec!["coding".to_string()]);
    // The new turn was started before `resume` returned.
    assert!(next.turn_gate().busy_for_task(&task.id) > 0);
    assert!(task_of(&q, &task.id).await.stuck_reason.is_none());
    q.release("job-c");
    q.wait_status(&task.id, "closed").await;
    let new = q
        .sessions(&task.id)
        .await
        .into_iter()
        .find(|s| s.id != old.id)
        .unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
}

#[tokio::test]
async fn a_paused_group_branch_with_a_completed_unapplied_turn_stays_paused_and_resume_reruns_its_session()
 {
    let q = Q::new(None).await;
    let g = paused_group(&q, &["two", "three"]).await;
    let old = session_of(&q, &g.id, "one").await;
    set_end(&q, &old.id, "idle", None, true).await;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&g.id).await, "paused");
    assert_eq!(
        reload(&q, &old.id).await.end_reason,
        Some(SessionEndReason::DaemonStopped)
    );
    plain_script(&q, "one");
    let outcome = next.resume_task(&g.id).await.unwrap();
    assert_eq!(outcome.resumed, vec!["one".to_string()]);
    assert!(next.turn_gate().busy_for_task(&g.id) > 0);
    q.release("one");
    let new = {
        crate::test_support::wait_until("the resumed session", || async {
            q.sessions(&g.id)
                .await
                .into_iter()
                .find(|s| s.stage == "one" && s.id != old.id)
                .ok_or("none yet".to_string())
        })
        .await
    };
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
}

#[tokio::test]
async fn resume_with_nothing_to_start_or_wait_for_marks_a_single_stage_stuck() {
    let q = Q::new(None).await;
    let (task, session) = paused_single(&q, "idle", None, true).await;
    let next = q.restarted_engine();
    let err = next.resume_task(&task.id).await.unwrap_err();
    let text = refused_text(&err);
    assert!(
        text.starts_with("stage 'coding' cannot resume: ")
            && text.contains(&session.id)
            && text.contains("idle")
            && text.contains("; nothing is running or queued to move the task on")
            && text.ends_with("; retry continues it (the task is now stuck)"),
        "{text}"
    );
    let t = task_of(&q, &task.id).await;
    assert_eq!(t.status, "stuck");
    assert!(t.paused_at.is_none());
    assert!(
        q.queue_events(&task.id, EventType::TaskResumed)
            .await
            .is_empty()
    );
    plain_script(&q, "job-r");
    q.release("job-r");
    next.retry_task(&task.id, RetryMode::Auto).await.unwrap();
    assert!(q.sessions(&task.id).await.len() >= 2);
}

#[tokio::test]
async fn resume_marks_a_group_stuck_when_no_branch_can_move_on() {
    // (d2-i) a failed run with no watcher.
    let q = Q::new(None).await;
    let g = paused_group(&q, &["two", "three"]).await;
    let one = session_of(&q, &g.id, "one").await;
    set_end(&q, &one.id, "exited", Some("no_report"), false).await;
    let next = q.restarted_engine();
    let err = next.resume_task(&g.id).await.unwrap_err();
    assert!(refused_text(&err).contains("'one'"), "{err}");
    assert_eq!(q.status(&g.id).await, "stuck");
    // (d2-ii) the slot was already recorded failed: only the safety net sees it.
    let q = Q::new(None).await;
    let g = paused_group(&q, &["two", "three"]).await;
    set_slot(&q, &g.id, "one", "failed").await;
    let next = q.restarted_engine();
    let err = next.resume_task(&g.id).await.unwrap_err();
    let text = refused_text(&err);
    assert!(
        text.contains("'one'") && text.contains("nothing is running or queued"),
        "{text}"
    );
    assert_eq!(q.status(&g.id).await, "stuck");
}

#[tokio::test]
async fn a_failed_branch_with_no_watcher_fails_the_plan_instead_of_restarting_its_siblings() {
    // (d2-iii)
    let q = Q::new(None).await;
    let g = paused_group(&q, &["three"]).await;
    let one = session_of(&q, &g.id, "one").await;
    set_end(&q, &one.id, "exited", Some("no_report"), false).await;
    let before = q.sessions(&g.id).await.len();
    let next = q.restarted_engine();
    let err = next.resume_task(&g.id).await.unwrap_err();
    assert!(refused_text(&err).contains("'one'"), "{err}");
    assert_eq!(q.status(&g.id).await, "stuck");
    assert_eq!(
        q.sessions(&g.id).await.len(),
        before,
        "two was not restarted"
    );
}

#[tokio::test]
async fn a_completed_unapplied_branch_with_no_watcher_fails_the_plan_instead_of_restarting_its_siblings()
 {
    let q = Q::new(None).await;
    let g = paused_group(&q, &["three"]).await;
    let one = session_of(&q, &g.id, "one").await;
    set_end(&q, &one.id, "idle", None, true).await;
    let two = session_of(&q, &g.id, "two").await;
    set_end(&q, &two.id, "exited", Some("paused"), true).await;
    let before = q.sessions(&g.id).await.len();
    let next = q.restarted_engine();
    let err = next.resume_task(&g.id).await.unwrap_err();
    assert!(refused_text(&err).contains("'one'"), "{err}");
    assert_eq!(q.status(&g.id).await, "stuck");
    assert_eq!(
        q.sessions(&g.id).await.len(),
        before,
        "two was not restarted"
    );
}

#[tokio::test]
async fn the_safety_net_is_one_write_and_never_writes_open() {
    // (d3)
    let q = Q::new(None).await;
    let (task, _) = paused_single(&q, "idle", None, true).await;
    sqlx::query(
        "CREATE TRIGGER refuse_stuck BEFORE UPDATE ON tasks WHEN NEW.status = 'stuck' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE open_writes (n INTEGER); \
         CREATE TRIGGER saw_open AFTER UPDATE ON tasks WHEN NEW.status = 'open' \
         BEGIN INSERT INTO open_writes VALUES (1); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    let next = q.restarted_engine();
    let err = next.resume_task(&task.id).await.unwrap_err();
    assert!(
        matches!(err, ResumeTaskError::StuckWriteFailed(_)),
        "{err:?}"
    );
    assert!(!err.to_string().contains("now stuck"), "{err}");
    assert_eq!(q.status(&task.id).await, "paused");
    let opens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM open_writes")
        .fetch_one(&q.pool)
        .await
        .unwrap();
    assert_eq!(opens, 0, "open was never written");
}

#[tokio::test]
async fn a_graceful_pause_holds_the_group_until_the_last_turn_has_applied_its_result() {
    // (e1)
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
    ]);
    let g = create_group_flow(&q, &group_flow(&["one", "two"]), "agroup2.yaml", "fix-job").await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    q.engine.pause_task(&g.id, false).await.unwrap();
    q.engine.turn_gate().hold_after_free("one");
    q.release("one");
    crate::test_support::wait_until("one to be settling", || async {
        let gate = q.engine.turn_gate();
        if gate.busy_for_task(&g.id) == 2 && gate.running_for_task(&g.id) == 1 {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await;
    q.release("two");
    wait_slot(&q, &g.id, "two", "done").await;
    crate::test_support::wait_until("only one settling", || async {
        if q.engine.turn_gate().busy_for_task(&g.id) == 1 {
            Ok(())
        } else {
            Err("busy".to_string())
        }
    })
    .await;
    // One's turn is still applying its result: a settle now must not pause.
    q.engine.settle_pause(&g.id).await;
    assert_eq!(q.status(&g.id).await, "pausing");
    assert!(
        q.queue_events(&g.id, EventType::TaskPaused)
            .await
            .is_empty(),
        "no task_paused while a turn is still settling"
    );
    q.engine.turn_gate().release_after_free("one");
    // Wait on the events asserted on, not on the status (#314).
    crate::test_support::wait_until("task_paused and one's branch_finished", || async {
        let events = q.events(&g.id).await;
        let paused = events.iter().any(|e| e.event_type == EventType::TaskPaused);
        let one = events
            .iter()
            .any(|e| e.event_type == EventType::BranchFinished && e.payload["branch"] == "one");
        if paused && one {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await;
    assert_eq!(q.status(&g.id).await, "paused");
    let events = q.events(&g.id).await;
    let paused: Vec<usize> = (0..events.len())
        .filter(|i| events[*i].event_type == EventType::TaskPaused)
        .collect();
    assert_eq!(paused.len(), 1);
    let one_done = (0..events.len())
        .filter(|i| {
            events[*i].event_type == EventType::BranchFinished
                && events[*i].payload["branch"] == "one"
        })
        .max()
        .unwrap();
    assert!(one_done < paused[0]);
}

#[tokio::test]
async fn a_settle_whose_task_paused_event_is_refused_leaves_the_task_pausing_and_a_later_settle_writes_both()
 {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_task_paused BEFORE INSERT ON events \
         WHEN NEW.event_type = 'task_paused' BEGIN SELECT RAISE(ABORT,'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    q.release("job-a");
    crate::test_support::wait_until("the turn to be gone", || async {
        if q.engine.turn_gate().busy_for_task(&a.id) == 0 {
            Ok(())
        } else {
            Err("busy".to_string())
        }
    })
    .await;
    let outcome = q.engine.pause_task(&a.id, true).await.unwrap();
    assert_eq!(outcome.status, "pausing");
    assert_eq!(q.status(&a.id).await, "pausing");
    assert!(
        q.queue_events(&a.id, EventType::TaskPaused)
            .await
            .is_empty()
    );

    sqlx::query("DROP TRIGGER refuse_task_paused")
        .execute(&q.pool)
        .await
        .unwrap();
    let outcome = q.engine.pause_task(&a.id, true).await.unwrap();
    assert_eq!(outcome.status, "paused");
    assert_eq!(q.status(&a.id).await, "paused");
    let stage = workflow_state::get(&q.pool, &a.id)
        .await
        .unwrap()
        .unwrap()
        .current_stage;
    let paused = q.queue_events(&a.id, EventType::TaskPaused).await;
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0]["stage"], json!(stage));
    assert!(!paused[0]["stage"].is_null());
}

#[tokio::test]
async fn a_restart_sweep_whose_task_paused_event_is_refused_leaves_the_task_pausing() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(q.status(&a.id).await, "pausing");
    sqlx::query(
        "CREATE TRIGGER refuse_task_paused BEFORE INSERT ON events \
         WHEN NEW.event_type = 'task_paused' BEGIN SELECT RAISE(ABORT,'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&a.id).await, "pausing");
    assert!(
        q.queue_events(&a.id, EventType::TaskPaused)
            .await
            .is_empty()
    );

    sqlx::query("DROP TRIGGER refuse_task_paused")
        .execute(&q.pool)
        .await
        .unwrap();
    next.park_interrupted_turns().await.unwrap();
    assert_eq!(q.status(&a.id).await, "paused");
    assert_eq!(
        q.queue_events(&a.id, EventType::TaskPaused).await,
        vec![json!({"restart": true})]
    );
}

const REFUSE_PAUSED_STATUS: &str = "CREATE TRIGGER refuse_paused_status BEFORE UPDATE ON tasks \
     WHEN NEW.status = 'paused' BEGIN SELECT RAISE(ABORT,'refused'); END";

#[tokio::test]
async fn a_settle_whose_paused_status_write_is_refused_writes_no_task_paused_event() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    sqlx::query(REFUSE_PAUSED_STATUS)
        .execute(&q.pool)
        .await
        .unwrap();
    q.release("job-a");
    crate::test_support::wait_until("the turn to be gone", || async {
        if q.engine.turn_gate().busy_for_task(&a.id) == 0 {
            Ok(())
        } else {
            Err("busy".to_string())
        }
    })
    .await;
    let outcome = q.engine.pause_task(&a.id, true).await.unwrap();
    assert_eq!(outcome.status, "pausing");
    assert_eq!(q.status(&a.id).await, "pausing");
    assert!(
        q.queue_events(&a.id, EventType::TaskPaused)
            .await
            .is_empty()
    );

    sqlx::query("DROP TRIGGER refuse_paused_status")
        .execute(&q.pool)
        .await
        .unwrap();
    let outcome = q.engine.pause_task(&a.id, true).await.unwrap();
    assert_eq!(outcome.status, "paused");
    assert_eq!(q.queue_events(&a.id, EventType::TaskPaused).await.len(), 1);
}

#[tokio::test]
async fn a_restart_sweep_whose_paused_status_write_is_refused_writes_no_task_paused_event() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    sqlx::query(REFUSE_PAUSED_STATUS)
        .execute(&q.pool)
        .await
        .unwrap();
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&a.id).await, "pausing");
    assert!(
        q.queue_events(&a.id, EventType::TaskPaused)
            .await
            .is_empty()
    );

    sqlx::query("DROP TRIGGER refuse_paused_status")
        .execute(&q.pool)
        .await
        .unwrap();
    next.park_interrupted_turns().await.unwrap();
    assert_eq!(q.status(&a.id).await, "paused");
    assert_eq!(
        q.queue_events(&a.id, EventType::TaskPaused).await,
        vec![json!({"restart": true})]
    );
}

#[tokio::test]
async fn a_stuck_mark_whose_error_event_is_refused_does_not_mark_the_task_stuck() {
    let q = Q::new(None).await;
    let (task, _) = paused_single(&q, "idle", None, true).await;
    sqlx::query(
        "CREATE TRIGGER refuse_stuck_event BEFORE INSERT ON events \
         WHEN NEW.event_type = 'error' AND json_extract(NEW.payload,'$.stuck') = 1 \
         BEGIN SELECT RAISE(ABORT,'refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();
    let err = q
        .restarted_engine()
        .resume_task(&task.id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ResumeTaskError::StuckWriteFailed(_)),
        "{err:?}"
    );
    assert_eq!(q.status(&task.id).await, "paused");
    assert!(task_of(&q, &task.id).await.stuck_reason.is_none());
    let stuck_errors = q
        .queue_events(&task.id, EventType::Error)
        .await
        .into_iter()
        .filter(|p| p["stuck"] == true)
        .count();
    assert_eq!(stuck_errors, 0);
}

#[tokio::test]
async fn a_settle_on_a_task_that_is_no_longer_pausing_writes_nothing() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    q.release("job-a");
    q.wait_status(&a.id, "paused").await;
    crate::test_support::wait_until("the task_paused event", || async {
        if q.queue_events(&a.id, EventType::TaskPaused).await.len() == 1 {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await;
    q.engine.settle_pause(&a.id).await;
    assert_eq!(q.status(&a.id).await, "paused");
    assert_eq!(q.queue_events(&a.id, EventType::TaskPaused).await.len(), 1);

    q.engine.resume_task(&a.id).await.unwrap();
    q.engine.settle_pause(&a.id).await;
    assert_eq!(q.status(&a.id).await, "open");
    assert_eq!(q.queue_events(&a.id, EventType::TaskPaused).await.len(), 1);
}

#[tokio::test]
async fn resume_while_a_completed_branch_is_still_settling_opens_and_does_not_mark_the_task_stuck()
{
    // A completed branch whose watcher is applying its result is running work.
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
    ]);
    let g = create_group_flow(&q, &group_flow(&["one", "two"]), "agroup3.yaml", "fix-job").await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    q.engine.pause_task(&g.id, false).await.unwrap();
    q.engine.turn_gate().hold_after_free("one");
    q.release("one");
    crate::test_support::wait_until("branch one to be settling", || async {
        let gate = q.engine.turn_gate();
        if gate.busy_for_stage(&g.id, "one") && gate.running_for_task(&g.id) == 1 {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await;
    let outcome = q.engine.resume_task(&g.id).await.unwrap();
    assert!(outcome.started.is_empty(), "{outcome:?}");
    assert_eq!(q.status(&g.id).await, "open");
    assert!(task_of(&q, &g.id).await.stuck_reason.is_none());
    q.engine.turn_gate().release_after_free("one");
    q.release("two");
    wait_slot(&q, &g.id, "one", "done").await;
    assert!(task_of(&q, &g.id).await.stuck_reason.is_none());
}

#[tokio::test]
async fn resume_during_a_turns_settling_gap_opens_and_the_result_is_applied() {
    // (e2)
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    q.engine.turn_gate().hold_after_free("coding");
    q.release("job-a");
    crate::test_support::wait_until("the turn to be settling", || async {
        let gate = q.engine.turn_gate();
        if gate.busy_for_task(&a.id) == 1 && gate.running_for_task(&a.id) == 0 {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await;
    assert_eq!(
        q.status(&a.id).await,
        "pausing",
        "not paused while settling"
    );
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert!(outcome.started.is_empty(), "{outcome:?}");
    assert_eq!(q.status(&a.id).await, "open");
    assert!(task_of(&q, &a.id).await.stuck_reason.is_none());
    q.engine.turn_gate().release_after_free("coding");
    q.wait_status(&a.id, "closed").await;
}

#[tokio::test]
async fn a_running_turn_counts_as_running_work_for_resume() {
    // (f1)
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert!(outcome.started.is_empty());
    assert_eq!(q.status(&a.id).await, "open");
    assert!(task_of(&q, &a.id).await.stuck_reason.is_none());
    q.release("job-a");
    q.wait_status(&a.id, "closed").await;
}

#[tokio::test]
async fn a_detached_runner_counts_as_running_work_for_resume() {
    // (f2)
    let q = Q::new(None).await;
    let a = create_flow(&q, &shell_flow(&q), "pshell.yaml", "sh").await;
    crate::test_support::wait_until("the command to start", || async {
        if q.dir.join("started-sh").exists() {
            Ok(())
        } else {
            Err("not started".to_string())
        }
    })
    .await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(q.status(&a.id).await, "pausing");
    let outcome = q.engine.resume_task(&a.id).await.unwrap();
    assert!(outcome.started.is_empty(), "{outcome:?}");
    assert_eq!(q.status(&a.id).await, "open");
    assert!(task_of(&q, &a.id).await.stuck_reason.is_none());
    fs::write(q.dir.join("release-sh"), "").unwrap();
    q.wait_status(&a.id, "closed").await;
}

#[tokio::test]
async fn a_paused_gate_and_a_paused_chat_stage_resume_open_with_nothing_started() {
    let q = Q::new(None).await;
    let gate = create_flow(&q, GATE_FLOW, "pgate.yaml", "gate").await;
    q.engine.pause_task(&gate.id, false).await.unwrap();
    assert_eq!(q.status(&gate.id).await, "paused");
    let outcome = q.engine.resume_task(&gate.id).await.unwrap();
    assert!(outcome.started.is_empty());
    assert_eq!(q.status(&gate.id).await, "open");
    assert!(task_of(&q, &gate.id).await.stuck_reason.is_none());

    let chat = "name: chat\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  talk:\n    kind: agent_turn\n    role: coder\n";
    let path = q.dir.join("chat.yaml");
    fs::write(&path, chat).unwrap();
    let task = q
        .task_with("talk", "open", &path.display().to_string(), json!({}))
        .await;
    let session = new_session(&q, &task, "talk").await;
    set_end(&q, &session.id, "idle", None, true).await;
    force_paused(&q, &task.id).await;
    let next = q.restarted_engine();
    let outcome = next.resume_task(&task.id).await.unwrap();
    assert!(outcome.started.is_empty(), "{outcome:?}");
    assert_eq!(q.status(&task.id).await, "open");
    assert!(task_of(&q, &task.id).await.stuck_reason.is_none());
}

const GATE_AFTER_AGENT: &str = "name: agate\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: { done: review }\n  review:\n    kind: human_gate\n    on: { resumed: finished }\n  finished:\n    kind: terminal\n";

async fn wait_idle_gate(q: &Q, id: &str) {
    crate::test_support::wait_until("the gate to hold no entry of the task", || async {
        if q.engine.turn_gate().busy_for_task(id) == 0 {
            Ok(())
        } else {
            Err("still busy".to_string())
        }
    })
    .await
}

#[tokio::test]
async fn the_settling_entry_is_removed_on_every_way_a_watcher_ends() {
    // completed, next stage a human gate
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, GATE_AFTER_AGENT, "agate.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.release("job-a");
    crate::test_support::wait_until("the gate stage", || async {
        if current_stage(&q, &a.id).await == "review" {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await;
    wait_idle_gate(&q, &a.id).await;
    let outcome = q.engine.pause_task(&a.id, false).await.unwrap();
    assert_eq!(outcome.status, "paused");

    // incomplete
    let q = Q::new(None).await;
    q.script(&[("job-b", Ending::NoReport)]);
    let b = q.create("job-b").await;
    q.wait_started("job-b").await;
    q.release("job-b");
    q.wait_status(&b.id, "stuck").await;
    wait_idle_gate(&q, &b.id).await;

    // pause --now
    let q = Q::new(None).await;
    q.script(&[("job-c", Ending::Report)]);
    let c = q.create("job-c").await;
    q.wait_started("job-c").await;
    q.engine.pause_task(&c.id, true).await.unwrap();
    q.wait_status(&c.id, "paused").await;
    wait_idle_gate(&q, &c.id).await;

    // a watcher that panics after freeing its slot
    let q = Q::new(None).await;
    q.script(&[("job-d", Ending::Report)]);
    q.engine.turn_gate().panic_after_free("coding");
    let d = q.create("job-d").await;
    q.wait_started("job-d").await;
    q.release("job-d");
    wait_idle_gate(&q, &d.id).await;
    assert_eq!(q.status(&d.id).await, "open");
    assert_eq!(current_stage(&q, &d.id).await, "coding");
    let outcome = q.engine.pause_task(&d.id, false).await.unwrap();
    assert_eq!(outcome.status, "paused");
}

#[tokio::test]
async fn a_paused_tasks_handled_end_is_not_judged_again_by_the_sweep() {
    let q = Q::new(None).await;
    let g = paused_group(&q, &[]).await;
    let before = task_of(&q, &g.id).await;
    // The branches' ends were handled by the pause. A read-only branch
    // changing the worktree afterwards is not the sweep's to judge.
    fs::write(group_worktree(&q, &g).join("changed.txt"), "x").unwrap();
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    let after = task_of(&q, &g.id).await;
    assert_eq!(after.status, "paused");
    assert!(after.stuck_reason.is_none());
    assert_eq!(after.paused_at, before.paused_at);
}

#[tokio::test]
async fn a_paused_task_at_a_held_stage_stays_paused_across_a_restart_and_resume_starts_it() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let a = create_flow(&q, PFLOW, "pflow.yaml", "job-a").await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&a.id, false).await.unwrap();
    q.release("job-a");
    q.wait_status(&a.id, "paused").await;
    let before = task_of(&q, &a.id).await;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    let after = task_of(&q, &a.id).await;
    assert_eq!(after.status, "paused");
    assert_eq!(after.paused_at, before.paused_at);
    let outcome = next.resume_task(&a.id).await.unwrap();
    assert_eq!(outcome.started, vec!["checks".to_string()]);
    q.wait_status(&a.id, "closed").await;
}

#[tokio::test]
async fn a_pausing_group_cut_off_by_a_crash_is_paused_and_resume_restarts_its_branches() {
    let q = Q::new(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = create_group_flow(&q, &three_group_flow(), "agroup3.yaml", "fix-job").await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
    }
    q.engine.pause_task(&g.id, false).await.unwrap();
    assert_eq!(q.status(&g.id).await, "pausing");
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    // Accepted cost: a turn that had completed would be re-run from its
    // session; these were running.
    assert_eq!(q.status(&g.id).await, "paused");
    for name in ["one", "two", "three"] {
        let s = session_of(&q, &g.id, name).await;
        assert_eq!(
            s.end_reason,
            Some(SessionEndReason::DaemonStopped),
            "{name}"
        );
        if s.adapter_session_id.is_none() {
            sessions::set_adapter_session_id(&q.pool, &s.id, &format!("adapter-{name}"))
                .await
                .unwrap();
        }
    }
    plain_script(&q, "rest");
    q.release("rest");
    let outcome = next.resume_task(&g.id).await.unwrap();
    let mut started = outcome.started.clone();
    started.sort();
    assert_eq!(started, ["one", "three", "two"].map(String::from).to_vec());
}
