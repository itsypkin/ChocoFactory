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

use crate::context_tool_tests::serve_context;
use chocofactory_core::context_tool::{ContextBinding, ContextClient};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A private daemon's address and the number of context requests it served.
struct Served {
    url: String,
    hits: Arc<AtomicUsize>,
}

impl Served {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Binds a listener now (so an engine can be told its address) and serves
/// the real context lookup over `pool` on it once the pool exists.
async fn bind_context_listener() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

/// What the tool answers for `launch`'s own binding: the recorded flags of
/// the launch, run through the same client the spawned CLI uses.
async fn ask(launch: &Launch, arguments: Value) -> Value {
    let anchor = launch.anchor.as_deref();
    assert_eq!(anchor.is_none(), launch.no_anchor);
    let binding = ContextBinding::new(&launch.task, &launch.session, anchor, &launch.base_url)
        .unwrap();
    let output = ContextClient::new(binding).unwrap().call(&arguments).await;
    assert!(!output.is_error, "{}", output.text);
    serde_json::from_str(&output.text).unwrap()
}

/// The `prep` departure as the launched turn must see it, and the task it
/// belongs to.
async fn assert_sees_prep(launch: &Launch, task: &Task, result_id: &str) {
    let got = ask(launch, json!({"source": "stage", "stage": "prep"})).await;
    assert_eq!(got["provenance"], "stage_result", "{got}");
    assert_eq!(got["task_id"], json!(task.id));
    assert_eq!(got["result"]["task_id"], json!(task.id));
    assert_eq!(got["result"]["result_id"], json!(result_id));
    assert_eq!(got["result"]["source_stage"], "prep");
    assert_eq!(got["result"]["destination_stage"], "coding");
    assert_eq!(got["anchor_id"], json!(result_id));
    let own = ask(launch, json!({"source": "task"})).await;
    assert_eq!(own["task_id"], json!(task.id));
    assert_eq!(own["task"]["title"], json!(task.title));
}

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

/// The launches the wrapper has logged. Each record is one `write(2)` of a
/// complete line to an append-only file, so only newline-terminated lines are
/// parsed (a line still being written is read on the next poll), and a
/// committed line that does not parse fails the test.
fn launches_in(log: &Path) -> Vec<Launch> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
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

/// A fixture whose engine is told the address of a private daemon that
/// really answers context lookups over its database.
async fn bound_q(max_agents: Option<u32>) -> (Q, Served) {
    let (listener, url) = bind_context_listener().await;
    let q = Q::build_with(max_agents, queue_timers(), true, Some(&url)).await;
    let hits = serve_context(listener, q.pool.clone());
    (q, Served { url, hits })
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
    let (q, daemon) = bound_q(None).await;
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
    assert_eq!(launch.base_url, format!("{}/", daemon.url));
    let session = q.sessions(&after.id).await.remove(0);
    assert_eq!(launch.session, session.id);
    assert_sees_prep(&launch, &after, &result).await;

    let entry = wait_launches(&q, &first.id, 1).await.remove(0);
    assert_eq!(entry.anchor, None);
    assert!(
        entry.no_anchor,
        "an entry turn has the explicit empty boundary"
    );
    assert_eq!(entry.session, q.sessions(&first.id).await.remove(0).id);
    // Its turn ends with a report and the task departs, but the turn's own
    // boundary was empty, so that later result stays invisible to it.
    q.release("job-c");
    q.wait_status(&first.id, "closed").await;
    assert!(!stage_results(&q.pool, &first.id).await.is_empty());
    for arguments in [
        json!({"source": "stage", "stage": "coding"}),
        json!({"source": "stage", "result_id": stage_results(&q.pool, &first.id).await[0].id}),
    ] {
        assert_eq!(
            ask(&entry, arguments).await,
            json!({"provenance": "unavailable", "task_id": first.id, "anchor_id": null,
                   "reason": "not_found_in_recorded_history"})
        );
    }

    let second = wait_launches(&q, &other.id, 1).await.remove(0);
    assert_ne!(second.session, launch.session);
    assert_ne!(second.anchor, launch.anchor, "another task's own result");
    let other_result = departure_of(&q, &other.id).await;
    assert_eq!(second.anchor.as_deref(), Some(other_result.as_str()));
    assert_sees_prep(&second, &other, &other_result).await;
    assert!(daemon.hits() > 0);
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
    let (q, daemon) = bound_q(Some(1)).await;
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
    let b_result = departure_of(&q, &b.id).await;
    assert_eq!(launch.anchor.as_deref(), Some(b_result.as_str()));
    let launch_a = wait_launches(&q, &a.id, 1).await.remove(0);
    assert_ne!(launch.anchor, launch_a.anchor);
    assert_eq!(launch.session, q.sessions(&b.id).await.remove(0).id);
    // Admitted after the slot opened, it reads its own task's history.
    assert_sees_prep(&launch, &b, &b_result).await;
    assert_sees_prep(&launch_a, &a, launch_a.anchor.as_deref().unwrap()).await;
    assert_eq!(
        stage_results(&q.pool, &a.id).await[0].id,
        launch_a.anchor.clone().unwrap(),
        "a's turn was bound to a's own first departure"
    );
    assert!(daemon.hits() > 0);
    q.release("job-b");
}

#[tokio::test]
async fn parallel_branches_get_their_own_sessions_and_keep_the_pointer_until_the_group_departs() {
    let (q, daemon) = bound_q(None).await;
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
    // Each branch's own binding reads the group's own task and its `prep`.
    for launch in &launches {
        let got = ask(launch, json!({"source": "stage", "stage": "prep"})).await;
        assert_eq!(got["result"]["task_id"], json!(g.id));
        assert_eq!(got["result"]["result_id"], json!(before));
        assert_eq!(got["result"]["destination_stage"], "panel");
        assert_eq!(ask(launch, json!({"source": "task"})).await["task"]["title"], json!(g.title));
    }
    assert_ne!(launches[0].session, launches[1].session);

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
    assert_eq!(
        ask(retried, json!({"source": "stage", "stage": "prep"})).await["result"]["result_id"],
        json!(before)
    );
    assert!(daemon.hits() > 0);
    q.release("two");
}

#[tokio::test]
async fn a_fresh_retry_gets_a_new_session_with_the_same_preceding_result() {
    let (q, daemon) = bound_q(None).await;
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
        assert_sees_prep(launch, &task, &result).await;
    }
    assert!(daemon.hits() > 0);
    q.release("job-a");
}

#[tokio::test]
async fn a_paused_turn_resumes_with_the_same_boundary_and_the_new_session() {
    let (q, daemon) = bound_q(None).await;
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
    // And the resumed turn reads the same preceding result through it.
    assert_sees_prep(resumed, &task, &result).await;
    assert!(daemon.hits() > 0);
}

#[tokio::test]
async fn a_restart_then_resume_keeps_the_boundary_and_binds_the_new_address() {
    let (q, daemon) = bound_q(None).await;
    q.script(&[("job-a", Ending::Report)]);
    let task = create_from(&q, &prep_flow("job-a.md"), "bind.yaml", "job-a", json!({})).await;
    q.wait_started("job-a").await;
    q.engine.pause_task(&task.id, false).await.unwrap();
    // The restarted daemon listens somewhere else; the old address stays up
    // (and counts) so a launch sent to it would show.
    let (listener, new_url) = bind_context_listener().await;
    let new_hits = serve_context(listener, q.pool.clone());
    let next = q.restarted_engine_at(Some(&new_url));
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
        format!("{}/", daemon.url),
        "history keeps its address"
    );
    assert_eq!(launches[1].base_url, format!("{new_url}/"));
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
    // Each launch reads from the daemon it was told about, not the other.
    assert_sees_prep(&launches[0], &task, &result).await;
    let (old_hits, new_hits_before) = (daemon.hits(), new_hits.load(Ordering::SeqCst));
    assert_eq!(new_hits_before, 0, "the old launch never reached the new daemon");
    assert_sees_prep(&launches[1], &task, &result).await;
    assert_eq!(daemon.hits(), old_hits, "the resumed launch never used the old address");
    assert!(new_hits.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn a_resume_with_no_provable_pointer_gets_an_explicit_empty_boundary_not_an_invented_one() {
    let (q, daemon) = bound_q(None).await;
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
    // A task from before results were recorded has no pointer in its state.
    sqlx::query("UPDATE workflow_state SET payload = json_remove(payload, '$.current_result_id') WHERE task_id = ?")
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!(held(&q.dir, "job-a", Ending::Report)).to_string(),
    )
    .unwrap();
    q.release("job-a");
    q.engine.resume_task(&task.id).await.unwrap();
    let launches = wait_launches(&q, &task.id, 2).await;
    let resumed = &launches[1];
    assert_eq!(resumed.resumed.as_deref(), Some("adapter-1"));
    assert!(resumed.no_anchor && resumed.anchor.is_none());
    // The result that does exist is not found through it, and no ID is made up.
    for arguments in [
        json!({"source": "stage", "stage": "prep"}),
        json!({"source": "stage", "result_id": result}),
    ] {
        assert_eq!(
            ask(resumed, arguments).await,
            json!({"provenance": "unavailable", "task_id": task.id, "anchor_id": null,
                   "reason": "not_found_in_recorded_history"})
        );
    }
    assert_eq!(ask(resumed, json!({"source": "task"})).await["task"]["title"], json!(task.title));
    assert!(daemon.hits() > 0);
}

#[tokio::test]
async fn a_damaged_pointer_refuses_the_launch_instead_of_becoming_an_empty_boundary() {
    let (q, _daemon) = bound_q(None).await;
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
async fn a_blank_pointer_refuses_the_launch_before_any_session_row_exists() {
    let (q, _daemon) = bound_q(None).await;
    q.script(&[("job-a", Ending::Crash)]);
    let task = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.release("job-a");
    q.wait_status(&task.id, "stuck").await;
    q.wait_running(0).await;
    let sessions_before = q.sessions(&task.id).await;
    sqlx::query("UPDATE workflow_state SET payload = json_set(payload, '$.current_result_id', '  ') WHERE task_id = ?")
        .bind(&task.id)
        .execute(&q.pool)
        .await
        .unwrap();
    // A launch that created a row and then failed to bind would settle it
    // with an `end_reason` write; refuse that write, so a cleanup that
    // cannot happen is visible instead of silently lost.
    sqlx::query(
        "CREATE TRIGGER refuse_cleanup BEFORE UPDATE ON sessions \
         WHEN NEW.end_reason = 'start_failed' \
         BEGIN SELECT RAISE(ABORT, 'cleanup refused'); END",
    )
    .execute(&q.pool)
    .await
    .unwrap();

    let err = q
        .engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("must not be blank"),
        "the blank pointer is named: {err}"
    );
    let after = q.sessions(&task.id).await;
    assert_eq!(
        after.iter().map(|s| &s.id).collect::<Vec<_>>(),
        sessions_before.iter().map(|s| &s.id).collect::<Vec<_>>(),
        "no session row was created for the refused launch"
    );
    assert!(
        after.iter().all(|s| s.status != SessionStatus::Active),
        "nothing is left active: {after:?}"
    );
}

#[tokio::test]
async fn a_standing_session_reestablished_by_a_message_is_bound_to_its_existing_session() {
    let pool = connect_in_memory().await.unwrap();
    let (first_listener, first_url) = bind_context_listener().await;
    let (second_listener, second_url) = bind_context_listener().await;
    let first_hits = serve_context(first_listener, pool.clone());
    let second_hits = serve_context(second_listener, pool.clone());
    let workflows_dir = tempdir();
    write_chat_workflow(&workflows_dir);
    let log = workflows_dir.join("argv.log");
    let wrapper = workflows_dir.join("claude-wrapper");
    crate::test_support::write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\npython3 -c 'import json,os,sys; os.write(1, (json.dumps(sys.argv[1:]) + chr(10)).encode())' \"$@\" >> '{}'\nexec '{}' \"$@\"\n",
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
    .with_context_base_url(&first_url)
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
    .with_context_base_url(&second_url)
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
    assert_eq!(launches[0].base_url, format!("{first_url}/"));
    let resumed = &launches[1];
    assert!(resumed.resumed.is_some(), "the message resumed the session");
    assert_eq!(
        resumed.session, session.id,
        "the existing session, not a new row"
    );
    assert_eq!(resumed.task, task.id);
    assert_eq!(resumed.base_url, format!("{second_url}/"));
    assert!(resumed.no_anchor, "a chat has recorded no stage result");
    // Read through the re-established binding: this task, this session's
    // boundary, and only the new daemon.
    let own = ask(resumed, json!({"source": "task"})).await;
    assert_eq!(own["task_id"], json!(task.id));
    assert_eq!(own["task"]["title"], json!(task.title));
    assert_eq!(
        ask(resumed, json!({"source": "stage", "stage": "chat"})).await["reason"],
        "not_found_in_recorded_history"
    );
    assert_eq!(first_hits.load(Ordering::SeqCst), 0);
    assert_eq!(second_hits.load(Ordering::SeqCst), 2);
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
