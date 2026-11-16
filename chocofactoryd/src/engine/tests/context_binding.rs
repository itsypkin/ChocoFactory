//! What each kind of launch hands the agent's `get_context` tool (#369): the
//! task, the session the launch actually created or resumed, the newest
//! stage result recorded before the turn, and the address this daemon is
//! really listening on.
//!
//! Read off the `--mcp-config` the engine really gave the spawned CLI: a
//! wrapper script records every launch's argv. The tool's own answers are
//! checked against real data in `adapter::context_tests`; here the question is
//! only which binding each launch path builds. These are isolated fixtures
//! (a scripted stand-in for `claude`), not runs of the real CLI.

use super::queue::{Ending, Q, held, queue_timers};
use super::*;

const URL: &str = "http://127.0.0.1:41999";
const OTHER_URL: &str = "http://127.0.0.1:42001";

/// `prep` (a shell stage) departs first, so the agent turn after it has a
/// preceding result; `FLOW`'s turn is the entry stage and has none.
fn prep_flow(prompt_file: &str) -> String {
    format!(
        "name: bindflow\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  prep:\n    kind: shell\n    command: \"echo ok\"\n    on: {{ done: coding }}\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: {prompt_file}\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
    )
}

/// A group entered after a shell stage.
const PREP_GROUP_FLOW: &str = "name: bindgroup\nworktree: true\nroles:\n  coder:\n    cli: claude\n    model: sonnet\n    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n  prep:\n    kind: shell\n    command: \"echo ok\"\n    on: { done: panel }\n  panel:\n    kind: parallel\n    branches:\n      one: { kind: agent_turn, role: coder, prompt_file: one.md }\n      two: { kind: agent_turn, role: coder, prompt_file: two.md }\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

/// One recorded launch of the stand-in CLI.
#[derive(Debug, Clone)]
struct Launch {
    task: String,
    session: String,
    anchor: Option<String>,
    no_anchor: bool,
    base_url: String,
    /// The adapter session `--resume` named, when the launch resumed one.
    resumed: Option<String>,
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn launches_in(log: &Path) -> Vec<Launch> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let argv: Vec<String> = serde_json::from_str(line).unwrap();
            let config: Value = serde_json::from_str(&value_after(&argv, "--mcp-config")?).unwrap();
            let mcp: Vec<String> = config["mcpServers"]["chocofactory"]["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap().to_string())
                .collect();
            Some(Launch {
                task: value_after(&mcp, "--context-task-id")?,
                session: value_after(&mcp, "--context-session-id").unwrap(),
                anchor: value_after(&mcp, "--context-anchor-id"),
                no_anchor: mcp.iter().any(|arg| arg == "--context-no-anchor"),
                base_url: value_after(&mcp, "--context-base-url").unwrap(),
                resumed: value_after(&argv, "--resume"),
            })
        })
        .collect()
}

async fn wait_launches(q: &Q, task_id: &str, count: usize) -> Vec<Launch> {
    crate::test_support::wait_until(&format!("{count} launch(es) of {task_id}"), || async {
        let all: Vec<Launch> = launches_in(&q.dir.join("argv.log"))
            .into_iter()
            .filter(|launch| launch.task == task_id)
            .collect();
        if all.len() >= count {
            Ok(all)
        } else {
            Err(format!("{} launch(es)", all.len()))
        }
    })
    .await
}

async fn bound_q(max_agents: Option<u32>) -> Q {
    Q::build_with(max_agents, queue_timers(), true, Some(URL)).await
}

/// A task running `yaml`, whose turn prompts hold `name` (the key the
/// scripted stand-in matches on).
async fn create_from(q: &Q, yaml: &str, file: &str, name: &str, config: Value) -> Task {
    let path = q.dir.join(file);
    fs::write(&path, yaml).unwrap();
    fs::write(q.dir.join(format!("{name}.md")), name).unwrap();
    q.engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(path),
            name,
            name,
            config,
            None,
            &[],
        )
        .await
        .unwrap()
}

async fn departure_of(q: &Q, task_id: &str) -> String {
    let results = stage_results(&q.pool, task_id).await;
    assert_eq!(results.len(), 1, "{results:?}");
    results[0].id.clone()
}

/// The `workflow_state` pointer, as the engine keeps it.
async fn pointer_of(q: &Q, task_id: &str) -> Option<String> {
    let state = workflow_state::get(&q.pool, task_id)
        .await
        .unwrap()
        .unwrap();
    chocofactory_core::context::current_result_id(&state.payload).unwrap()
}

#[tokio::test]
async fn a_launch_is_bound_to_its_own_task_session_preceding_result_and_the_daemons_address() {
    let q = bound_q(None).await;
    q.script(&[
        ("job-a", Ending::Report),
        ("job-b", Ending::Report),
        ("job-c", Ending::Report),
    ]);
    // A turn entered after a departure, a turn that is the entry stage, and
    // a second task like the first: three tasks, three sessions.
    let after = create_from(&q, &prep_flow("job-a.md"), "bind.yaml", "job-a", json!({})).await;
    let first = q.create("job-c").await;
    let other = create_from(&q, &prep_flow("job-b.md"), "bind2.yaml", "job-b", json!({})).await;
    for name in ["a", "b", "c"] {
        q.wait_started(&format!("job-{name}")).await;
    }

    let launch = wait_launches(&q, &after.id, 1).await.remove(0);
    let result = departure_of(&q, &after.id).await;
    assert_eq!(launch.anchor.as_deref(), Some(result.as_str()));
    assert!(!launch.no_anchor);
    assert_eq!(launch.base_url, format!("{URL}/"));
    let session = q.sessions(&after.id).await.remove(0);
    assert_eq!(launch.session, session.id);

    let entry = wait_launches(&q, &first.id, 1).await.remove(0);
    assert_eq!(entry.anchor, None);
    assert!(
        entry.no_anchor,
        "an entry turn has the explicit empty boundary"
    );
    assert_eq!(entry.session, q.sessions(&first.id).await.remove(0).id);

    let second = wait_launches(&q, &other.id, 1).await.remove(0);
    assert_ne!(second.session, launch.session);
    assert_ne!(second.anchor, launch.anchor, "another task's own result");
    assert_eq!(
        second.anchor.as_deref(),
        Some(departure_of(&q, &other.id).await.as_str())
    );
    for name in ["job-a", "job-b", "job-c"] {
        q.release(name);
    }
}

#[tokio::test]
async fn an_engine_never_told_its_address_builds_no_binding_and_a_told_one_builds_it() {
    let pool = connect_in_memory().await.unwrap();
    let unbound = engine_with_adapter(pool.clone(), "unused");
    let bound = engine_with_adapter(pool.clone(), "unused")
        .with_context_base_url(URL)
        .unwrap();
    assert_eq!(unbound.launch_context("t", "s", Some("r")), Ok(None));
    let binding = bound.launch_context("t", "s", Some("r")).unwrap().unwrap();
    assert_eq!(binding.task_id, "t");
    assert_eq!(binding.session_id, "s");
    assert_eq!(binding.anchor_id.as_deref(), Some("r"));
    // A bad address fails when the engine is told, not at every launch.
    for bad in ["", "127.0.0.1:4141", "ftp://x", "http://h/?q=1"] {
        let engine = engine_with_adapter(pool.clone(), "unused");
        assert!(engine.with_context_base_url(bad).is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn a_queued_turn_is_bound_when_admitted_to_its_own_session_and_result() {
    let q = bound_q(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    let a = create_from(&q, &prep_flow("job-a.md"), "bind.yaml", "job-a", json!({})).await;
    q.wait_started("job-a").await;
    let b = create_from(&q, &prep_flow("job-b.md"), "bind2.yaml", "job-b", json!({})).await;
    q.wait_rows(1).await;
    // Queued: nothing launched for it yet.
    assert!(
        launches_in(&q.dir.join("argv.log"))
            .iter()
            .all(|l| l.task != b.id)
    );
    q.release("job-a");
    q.wait_started("job-b").await;
    let launch = wait_launches(&q, &b.id, 1).await.remove(0);
    assert_eq!(
        launch.anchor.as_deref(),
        Some(departure_of(&q, &b.id).await.as_str())
    );
    assert_ne!(launch.anchor, wait_launches(&q, &a.id, 1).await[0].anchor);
    assert_eq!(launch.session, q.sessions(&b.id).await.remove(0).id);
    q.release("job-b");
}

#[tokio::test]
async fn parallel_branches_get_their_own_sessions_and_keep_the_pointer_until_the_group_departs() {
    let q = bound_q(None).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Crash),
    ]);
    let repo = q.dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo).await;
    let g = create_from(
        &q,
        PREP_GROUP_FLOW,
        "bindgroup.yaml",
        "go",
        json!({ "cwd": repo.to_string_lossy() }),
    )
    .await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    let before = pointer_of(&q, &g.id).await.expect("prep departed");
    let launches = wait_launches(&q, &g.id, 2).await;
    let sessions = q.sessions(&g.id).await;
    assert_eq!(sessions.len(), 2);
    let branch_sessions: std::collections::HashSet<_> =
        launches.iter().map(|l| l.session.clone()).collect();
    assert_eq!(branch_sessions.len(), 2, "one session per branch");
    for session in &sessions {
        assert!(branch_sessions.contains(&session.id));
    }
    assert!(
        launches
            .iter()
            .all(|l| l.anchor.as_deref() == Some(before.as_str()))
    );

    // One branch finishes, the other fails and is retried: the pointer has
    // not moved (the group has not departed) and neither has the anchor.
    q.release("one");
    q.release("two");
    q.wait_status(&g.id, "stuck").await;
    assert_eq!(pointer_of(&q, &g.id).await, Some(before.clone()));
    q.script_branches(&[("BRANCH-TWO", "two", Ending::Report)]);
    fs::remove_file(q.dir.join("release-two")).unwrap();
    q.engine.retry_task(&g.id, RetryMode::Fresh).await.unwrap();
    let launches = wait_launches(&q, &g.id, 3).await;
    let retried = launches.last().unwrap();
    assert_eq!(retried.anchor.as_deref(), Some(before.as_str()));
    assert!(!branch_sessions.contains(&retried.session));
    q.release("two");
}

#[tokio::test]
async fn a_fresh_retry_gets_a_new_session_with_the_same_preceding_result() {
    let q = bound_q(None).await;
    q.script(&[("job-a", Ending::Crash)]);
    let task = create_from(&q, &prep_flow("job-a.md"), "bind.yaml", "job-a", json!({})).await;
    q.wait_started("job-a").await;
    q.release("job-a");
    q.wait_status(&task.id, "stuck").await;
    let result = departure_of(&q, &task.id).await;
    fs::remove_file(q.dir.join("release-job-a")).unwrap();
    fs::remove_file(q.dir.join("started-job-a")).unwrap();
    q.script(&[("job-a", Ending::Report)]);
    q.engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap();
    let launches = wait_launches(&q, &task.id, 2).await;
    assert_ne!(launches[0].session, launches[1].session);
    assert_eq!(launches[1].resumed, None, "a fresh retry resumes nothing");
    for launch in &launches {
        assert_eq!(launch.anchor.as_deref(), Some(result.as_str()));
    }
    q.release("job-a");
}

#[tokio::test]
async fn a_paused_turn_resumes_with_the_same_boundary_and_the_new_session() {
    let q = bound_q(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let task = create_from(&q, &prep_flow("job-a.md"), "bind.yaml", "job-a", json!({})).await;
    q.wait_started("job-a").await;
    let result = departure_of(&q, &task.id).await;
    q.engine.pause_task(&task.id, true).await.unwrap();
    q.wait_status(&task.id, "paused").await;
    let old = q.sessions(&task.id).await.remove(0);
    sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
        .await
        .unwrap();
    // The pause departed nothing: the pointer is exactly what it was.
    assert_eq!(pointer_of(&q, &task.id).await, Some(result.clone()));

    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!(held(&q.dir, "job-a", Ending::Report)).to_string(),
    )
    .unwrap();
    q.release("job-a");
    let outcome = q.engine.resume_task(&task.id).await.unwrap();
    assert_eq!(outcome.resumed, vec!["coding".to_string()]);
    let launches = wait_launches(&q, &task.id, 2).await;
    let resumed = &launches[1];
    // The resume really happened: the CLI was told to continue the adapter
    // session, and the new session row points back at the old one.
    assert_eq!(resumed.resumed.as_deref(), Some("adapter-1"));
    let new = q
        .sessions(&task.id)
        .await
        .into_iter()
        .find(|s| s.id != old.id)
        .unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
    assert_eq!(resumed.session, new.id, "bound to the resumed session");
    assert_eq!(resumed.anchor, launches[0].anchor);
    assert_eq!(resumed.anchor.as_deref(), Some(result.as_str()));
}

#[tokio::test]
async fn a_restart_then_resume_keeps_the_boundary_and_binds_the_new_address() {
    let q = bound_q(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let task = create_from(&q, &prep_flow("job-a.md"), "bind.yaml", "job-a", json!({})).await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&task.id, false).await.unwrap();
    let next = q.restarted_engine_at(Some(OTHER_URL));
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&task.id).await, "paused");
    let old = q.sessions(&task.id).await.remove(0);
    assert_eq!(old.end_reason, Some(SessionEndReason::DaemonStopped));
    sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
        .await
        .unwrap();
    let result = departure_of(&q, &task.id).await;
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!(held(&q.dir, "job-a", Ending::Report)).to_string(),
    )
    .unwrap();
    q.release("job-a");
    next.resume_task(&task.id).await.unwrap();

    let launches = wait_launches(&q, &task.id, 2).await;
    assert_eq!(
        launches[0].base_url,
        format!("{URL}/"),
        "history keeps its address"
    );
    assert_eq!(launches[1].base_url, format!("{OTHER_URL}/"));
    assert_eq!(launches[1].resumed.as_deref(), Some("adapter-1"));
    assert_eq!(launches[1].anchor.as_deref(), Some(result.as_str()));
    assert_eq!(launches[1].anchor, launches[0].anchor);
    let new = q
        .sessions(&task.id)
        .await
        .into_iter()
        .find(|s| s.id != old.id)
        .unwrap();
    assert_eq!(launches[1].session, new.id);
}

#[tokio::test]
async fn a_damaged_pointer_refuses_the_launch_instead_of_becoming_an_empty_boundary() {
    let q = bound_q(None).await;
    q.script(&[("job-a", Ending::Crash)]);
    let task = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.release("job-a");
    q.wait_status(&task.id, "stuck").await;
    q.wait_running(0).await;
    let sessions_before = q.sessions(&task.id).await.len();
    let state = workflow_state::get(&q.pool, &task.id)
        .await
        .unwrap()
        .unwrap();
    let mut payload = state.payload.clone();
    payload["current_result_id"] = json!(7);
    sqlx::query("UPDATE workflow_state SET payload = ? WHERE task_id = ?")
        .bind(payload.to_string())
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();

    let err = q
        .engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("damaged"),
        "an honest refusal, not an empty boundary: {err}"
    );
    assert_eq!(q.sessions(&task.id).await.len(), sessions_before);
    let launched: Vec<_> = launches_in(&q.dir.join("argv.log"))
        .into_iter()
        .filter(|launch| launch.task == task.id)
        .collect();
    assert_eq!(launched.len(), 1, "no second launch");
}

#[tokio::test]
async fn a_standing_session_reestablished_by_a_message_is_bound_to_its_existing_session() {
    let pool = connect_in_memory().await.unwrap();
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let log = workflows_dir.join("argv.log");
    let wrapper = workflows_dir.join("claude-wrapper");
    crate::test_support::write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\npython3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' \"$@\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            fixture_binary("fake_claude.py"),
        ),
    );
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_adapter_and_workflows_dir(
        pool.clone(),
        &wrapper.display().to_string(),
        &workflows_dir,
    )
    .with_context_base_url(URL)
    .unwrap();
    let task = engine
        .create_task(&project_id, "chat", "t", "hello", json!({}))
        .await
        .unwrap();
    let session = sessions::list_for_task(&pool, &task.id)
        .await
        .unwrap()
        .remove(0);
    crate::test_support::wait_until("the chat session to report its adapter id", || async {
        match sessions::get(&pool, &session.id).await.unwrap().unwrap() {
            s if s.adapter_session_id.is_some() => Ok(()),
            _ => Err("no adapter session id yet".to_string()),
        }
    })
    .await;

    // A restarted daemon on another address: the session is idle, not live.
    sessions::recover_stale_active_sessions(&pool)
        .await
        .unwrap();
    let adapter: Arc<dyn AgentAdapter> =
        Arc::new(ClaudeAdapter::with_binary(wrapper.display().to_string()));
    let events_notify = Arc::new(Notify::new());
    let manager = SessionManager::new(
        pool.clone(),
        Registry::single(adapter),
        chrono::Duration::hours(1),
        Arc::clone(&events_notify),
    );
    let next = WorkflowEngine::new(
        pool.clone(),
        manager,
        workflows_dir.to_path_buf(),
        None,
        events_notify,
    )
    .with_context_base_url(OTHER_URL)
    .unwrap();
    next.send_message_or_resume(&task.id, "are you still there?")
        .await
        .unwrap();

    let launches = crate::test_support::wait_until("the resumed launch", || async {
        let all = launches_in(&log);
        if all.len() >= 2 {
            Ok(all)
        } else {
            Err(format!("{} launch(es)", all.len()))
        }
    })
    .await;
    assert_eq!(launches[0].base_url, format!("{URL}/"));
    let resumed = &launches[1];
    assert!(resumed.resumed.is_some(), "the message resumed the session");
    assert_eq!(
        resumed.session, session.id,
        "the existing session, not a new row"
    );
    assert_eq!(resumed.task, task.id);
    assert_eq!(resumed.base_url, format!("{OTHER_URL}/"));
    assert!(resumed.no_anchor, "a chat has recorded no stage result");
    assert_eq!(
        sessions::list_for_task(&pool, &task.id)
            .await
            .unwrap()
            .len(),
        1,
        "no second session row"
    );

    // A damaged pointer is refused rather than read as an empty boundary,
    // and nothing is relayed or recorded for the refused message.
    let human_messages = || async {
        events::list_for_session(&pool, &session.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::HumanMessage)
            .count()
    };
    let messages_before = human_messages().await;
    assert_eq!(messages_before, 2, "the first message and the resumed one");
    sqlx::query("UPDATE workflow_state SET payload = ? WHERE task_id = ?")
        .bind(json!({"current_result_id": 7}).to_string())
        .bind(&task.id)
        .execute(&pool)
        .await
        .unwrap();
    let err = next
        .send_message_or_resume(&task.id, "and now?")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SendMessageOrResumeError::SendMessage(SendMessageError::ContextBinding(_))
        ),
        "{err:?}"
    );
    assert_eq!(human_messages().await, messages_before);
}
