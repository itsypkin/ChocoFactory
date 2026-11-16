//! A `get_context` read never finishes, reports or advances a turn (#369).
//!
//! Through the real engine, session manager and adapter for each CLI, driven
//! by scripted stand-ins (`fake_claude_script.py`, `fake_omp.py`), not the
//! real CLIs. Each read is followed by what a turn really does next — more
//! work and a report, or a clean end with no report — and the assertions are
//! on the task's settled state: its stage, its recorded outcome and its
//! session's end.

use super::omp_wake::{WAKE_YAML, assert_not_applied, start_wake_with, timers};
use super::queue::Q;
use super::*;
use crate::context_tool_tests::decoy_daemon;
use chocofactory_core::models::SessionEndReason;

/// `sleep`-free timers: a report-less turn is nudged once and then given up
/// on in well under a second.
fn quick_timers() -> crate::session::TurnTimers {
    crate::session::TurnTimers {
        grace: StdDuration::from_millis(400),
        nudge_after: StdDuration::from_millis(300),
        max_nudges: 1,
        job_wait_limit: StdDuration::from_secs(60),
    }
}

const QUALIFIED: &str = "mcp__chocofactory__get_context";

fn context_call() -> Value {
    json!({"source": "task"})
}

/// The `tool` of every tool event a session recorded, in order, with its
/// kind.
async fn tool_events(pool: &SqlitePool, session_id: &str) -> Vec<(EventType, String, String)> {
    events::list_for_session(pool, session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.event_type, EventType::ToolCall | EventType::ToolResult))
        .map(|e| {
            (
                e.event_type,
                e.payload["tool"].as_str().unwrap_or("").to_string(),
                e.payload["tool_use_id"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

async fn wait_for_assistant_text(pool: &SqlitePool, session_id: &str, text: &str) {
    crate::test_support::wait_until(&format!("'{text}' to be recorded"), || async {
        let seen = events::list_for_session(pool, session_id)
            .await
            .unwrap()
            .into_iter()
            .any(|e| {
                e.event_type == EventType::AssistantMessage
                    && e.payload["text"].as_str() == Some(text)
            });
        if seen {
            Ok(())
        } else {
            Err("not yet".to_string())
        }
    })
    .await
}

fn only_context_events(
    events: &[(EventType, String, String)],
) -> Vec<&(EventType, String, String)> {
    events
        .iter()
        .filter(|(_, tool, _)| tool.ends_with("get_context"))
        .collect()
}

/// The pair every read leaves: one qualified call and one result with its ID,
/// and nothing under the bare name.
fn assert_one_qualified_pair(events: &[(EventType, String, String)], reads: usize) {
    let context = only_context_events(events);
    assert_eq!(context.len(), 2 * reads, "{events:?}");
    for (_, tool, _) in &context {
        assert_eq!(tool, QUALIFIED, "{events:?}");
    }
    for pair in context.chunks(2) {
        assert_eq!(pair[0].0, EventType::ToolCall);
        assert_eq!(pair[1].0, EventType::ToolResult);
        assert_eq!(pair[0].2, pair[1].2, "a result carries its call's ID");
    }
}

// ---- omp -------------------------------------------------------------------

fn omp_steps(read_args: Value, tail: Vec<Value>) -> Value {
    let mut steps = vec![
        json!({"op": "pid"}),
        json!({"op": "agent_start"}),
        json!({"op": "context", "args": read_args, "label": "1"}),
        json!({"op": "assistant", "text": "more work after the read"}),
    ];
    steps.extend(tail);
    Value::Array(steps)
}

#[tokio::test]
async fn omp_a_read_then_more_work_then_a_report_completes_the_turn_once() {
    let decoy = decoy_daemon().await;
    let run = start_wake_with(
        WAKE_YAML,
        omp_steps(
            context_call(),
            vec![
                json!({"op": "gate", "name": "go"}),
                json!({"op": "report", "args": {"outcome": "changes_requested", "summary": "s"}}),
                json!({"op": "assistant", "text": "{\"outcome\": \"changes_requested\"}"}),
                json!({"op": "agent_end"}),
                json!({"op": "session_settled"}),
            ],
        ),
        timers(),
        Some(&decoy),
    )
    .await;
    let session = run.session().await;
    // The read has been answered, and the agent's later work is recorded —
    // by which point the read's events were all taken in.
    run.script.wait_log("context_result:1:ok").await;
    wait_for_assistant_text(&run.pool, &session.id, "more work after the read").await;
    let task = tasks::get(&run.pool, &run.task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "open");
    assert_eq!(
        stage_trail(&run.pool, &run.task_id).await,
        vec![("review".to_string(), Value::Null)],
        "{}",
        run.describe().await
    );
    assert!(run.outcomes().await.is_empty());

    run.script.release("go");
    wait_until_stage(&run.pool, &run.task_id, "bounced").await;
    let outcome = wait_until_turn_outcome_event(&run.pool, &run.task_id).await;
    assert_eq!(outcome["outcome"], "changes_requested", "{outcome}");
    assert_eq!(outcome["applied"], true);
    wait_until_task_status(&run.pool, &run.task_id, "closed").await;
    assert_eq!(run.outcomes().await.len(), 1, "applied once");
    // The read was one qualified pair, the report another.
    let tools = tool_events(&run.pool, &session.id).await;
    assert_one_qualified_pair(&tools, 1);
    assert!(
        tools
            .iter()
            .any(|(_, tool, _)| tool == "mcp__chocofactory__report_outcome"),
        "{tools:?}"
    );
}

#[tokio::test]
async fn omp_a_clean_end_after_only_reads_is_no_report_not_completion() {
    let decoy = decoy_daemon().await;
    // A successful read, then a failed one (no daemon at port 1), each on
    // its own run: neither may stand in for a report.
    for (what, base_url) in [
        ("a successful read", decoy.as_str()),
        ("a failed read", "http://127.0.0.1:1"),
    ] {
        let run = start_wake_with(
            WAKE_YAML,
            omp_steps(
                context_call(),
                vec![json!({"op": "agent_end"}), json!({"op": "session_settled"})],
            ),
            quick_timers(),
            Some(base_url),
        )
        .await;
        assert_not_applied(&run, what).await;
        let session = run.session().await;
        crate::test_support::wait_until("the session to end", || async {
            let s = sessions::get(&run.pool, &session.id)
                .await
                .unwrap()
                .unwrap();
            if s.status == SessionStatus::Exited {
                Ok(s)
            } else {
                Err(format!("{:?}", s.status))
            }
        })
        .await;
        let ended = sessions::get(&run.pool, &session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ended.end_reason, Some(SessionEndReason::NoReport), "{what}");
        // Not recorded as a result, a capture or an outcome.
        assert!(
            stage_results(&run.pool, &run.task_id).await.is_empty(),
            "{what}"
        );
        let payload = payload_of(&run.pool, &run.task_id).await;
        assert!(
            payload.get("current_result_id").is_none(),
            "{what}: {payload}"
        );
        assert_one_qualified_pair(&tool_events(&run.pool, &session.id).await, 1);
    }
}

// ---- claude ------------------------------------------------------------------

fn claude_read_script(q: &Q, name: &str, steps: Vec<Value>) {
    let mut all = vec![
        json!({"op": "read_turn"}),
        json!({"op": "run", "command": format!("touch '{}'", q.dir.join(format!("started-{name}")).display())}),
    ];
    all.extend(steps);
    q.names.lock().unwrap().push(name.to_string());
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!({ "by_prompt": { name: all } }).to_string(),
    )
    .unwrap();
}

async fn session_of(q: &Q, task_id: &str) -> Session {
    q.sessions(task_id).await.remove(0)
}

#[tokio::test]
async fn claude_a_read_then_more_work_then_a_report_completes_the_turn_once() {
    let q = Q::new(None).await;
    claude_read_script(
        &q,
        "job-a",
        vec![
            json!({"op": "context"}),
            json!({"op": "text", "text": "more work after the read"}),
            json!({"op": "wait_file", "path": q.dir.join("release-job-a").display().to_string()}),
            json!({"op": "report", "outcome": "done"}),
            json!({"op": "result"}),
        ],
    );
    let task = q.create("job-a").await;
    q.wait_started("job-a").await;
    let session = crate::test_support::wait_until("the session", || async {
        q.sessions(&task.id)
            .await
            .into_iter()
            .next()
            .ok_or("none".to_string())
    })
    .await;
    wait_for_assistant_text(&q.pool, &session.id, "more work after the read").await;
    assert_eq!(q.status(&task.id).await, "open");
    assert!(stage_results(&q.pool, &task.id).await.is_empty());

    q.release("job-a");
    q.wait_status(&task.id, "closed").await;
    // One departure, applying the report's outcome.
    let results = stage_results(&q.pool, &task.id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].payload["source_stage"], "coding");
    assert_eq!(results[0].payload["outcome"], "done");
    assert_one_qualified_pair(&tool_events(&q.pool, &session.id).await, 1);
}

#[tokio::test]
async fn claude_a_clean_end_after_only_reads_is_no_report_not_completion() {
    for (what, is_error) in [("a successful read", false), ("a failed read", true)] {
        let q = Q::build(None, quick_timers(), true).await;
        claude_read_script(
            &q,
            "job-a",
            vec![
                json!({"op": "context", "is_error": is_error}),
                json!({"op": "result"}),
                json!({"op": "answer_every_turn", "text": "thinking"}),
            ],
        );
        let task = q.create("job-a").await;
        q.wait_status(&task.id, "stuck").await;
        let session = session_of(&q, &task.id).await;
        crate::test_support::wait_until("the session to end", || async {
            let s = sessions::get(&q.pool, &session.id).await.unwrap().unwrap();
            if s.status == SessionStatus::Exited {
                Ok(s)
            } else {
                Err(format!("{:?}", s.status))
            }
        })
        .await;
        let ended = sessions::get(&q.pool, &session.id).await.unwrap().unwrap();
        assert_eq!(ended.end_reason, Some(SessionEndReason::NoReport), "{what}");
        assert!(stage_results(&q.pool, &task.id).await.is_empty(), "{what}");
        assert!(
            q.queue_events(&task.id, EventType::TurnOutcome)
                .await
                .is_empty(),
            "{what}"
        );
        assert_eq!(
            workflow_state::get(&q.pool, &task.id)
                .await
                .unwrap()
                .unwrap()
                .current_stage,
            "coding"
        );
        assert_one_qualified_pair(&tool_events(&q.pool, &session.id).await, 1);
    }
}
