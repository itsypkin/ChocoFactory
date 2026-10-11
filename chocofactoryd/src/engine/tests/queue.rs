//! Capping agent turns (`max_agents`): the gate, the queue and the dispatcher.
//!
//! The fake agent holds a turn (and so its slot) until the test creates that
//! turn's release file, which makes "who holds a slot now" a thing a test
//! decides.

use super::*;
use crate::db::turn_queue::{self, QueueRow};
use crate::engine::turns::{Claim, Pass, RowFate, TurnSpec, is_admissible, queue_row_fate};
use chocofactory_core::models::Project;

const FLOW: &str = "name: qflow\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

const GROUP_FLOW: &str = "name: qgroup\nworktree: true\nroles:\n  coder:\n    cli: claude\n    model: sonnet\n    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\nstages:\n  panel:\n    kind: parallel\n    branches:\n      one: { kind: agent_turn, role: coder, prompt_file: one.md }\n      two: { kind: agent_turn, role: coder, prompt_file: two.md }\n      three: { kind: agent_turn, role: coder, prompt_file: three.md }\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

/// What a held turn does after its release file appears.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ending {
    /// Reports `done` and ends.
    Report,
    /// Exits without reporting.
    Crash,
    /// Ends its turn without reporting, however often it is nudged.
    NoReport,
    /// Hits the account's usage limit.
    UsageLimit,
    /// Reports, then leaves a child running past the grace period.
    Lingers,
}

fn held(dir: &Path, name: &str, ending: Ending) -> Vec<Value> {
    let mut steps = vec![
        json!({"op": "read_turn"}),
        json!({"op": "run", "command": format!("touch '{}'", dir.join(format!("started-{name}")).display())}),
        json!({"op": "wait_file", "path": dir.join(format!("release-{name}")).display().to_string()}),
    ];
    match ending {
        Ending::Report => {
            steps.push(json!({"op": "report", "outcome": "done"}));
            steps.push(json!({"op": "result"}));
        }
        Ending::Crash => steps.push(json!({"op": "exit", "code": 3})),
        Ending::NoReport => {
            steps.push(json!({"op": "result"}));
            steps.push(json!({"op": "answer_every_turn", "text": "thinking"}));
        }
        Ending::UsageLimit => steps.push(json!({"op": "usage_limit"})),
        Ending::Lingers => {
            steps.push(json!({"op": "report", "outcome": "done"}));
            steps.push(json!({"op": "result"}));
            steps.push(json!({
                "op": "spawn_child",
                "heartbeat": dir.join(format!("heartbeat-{name}")).display().to_string(),
                "pid_file": dir.join(format!("child-{name}.pid")).display().to_string(),
            }));
        }
    }
    steps
}

struct Q {
    pool: SqlitePool,
    engine: Arc<WorkflowEngine>,
    dir: TempDir,
    config: PathBuf,
    flow: PathBuf,
    project: Project,
    names: std::sync::Mutex<Vec<String>>,
    dispatcher: Option<JoinHandle<()>>,
}

impl Drop for Q {
    /// Lets every fake agent still waiting go, so no process outlives its test.
    fn drop(&mut self) {
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.abort();
        }
        for name in self.names.lock().unwrap().iter() {
            let _ = fs::write(self.dir.join(format!("release-{name}")), "");
        }
    }
}

fn queue_timers() -> crate::session::TurnTimers {
    crate::session::TurnTimers {
        grace: StdDuration::from_millis(300),
        nudge_after: StdDuration::from_millis(200),
        max_nudges: 1,
        job_wait_limit: StdDuration::from_secs(60),
    }
}

impl Q {
    /// `max_agents` is written into `config.yaml` when given.
    async fn new(max_agents: Option<u32>) -> Q {
        Q::build(max_agents, queue_timers(), true).await
    }

    async fn build(
        max_agents: Option<u32>,
        timers: crate::session::TurnTimers,
        dispatch: bool,
    ) -> Q {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let config = dir.join("config.yaml");
        if let Some(n) = max_agents {
            fs::write(&config, format!("max_agents: {n}\n")).unwrap();
        }
        let flow = dir.join("qflow.yaml");
        fs::write(&flow, FLOW).unwrap();
        fs::write(dir.join("qgroup.yaml"), GROUP_FLOW).unwrap();
        for branch in ["one", "two", "three"] {
            fs::write(
                dir.join(format!("{branch}.md")),
                format!("BRANCH-{}", branch.to_uppercase()),
            )
            .unwrap();
        }
        let project = projects::create(&pool, "demo", None).await.unwrap();
        let binary = named_script_binary(&dir, "fake-claude-queue", json!({"by_prompt": {}}));
        // The wrapper logs the moment each agent process ends, so a test can
        // tell when a slot's process was really gone.
        let script = dir.join("fake-claude-queue.json");
        crate::test_support::write_executable(
            &dir.join("fake-claude-queue"),
            format!(
                "#!/bin/sh\nFAKE_CLAUDE_SCRIPT='{}' '{}' \"$@\"\ncode=$?\npython3 -c 'import time; print(time.time())' >> '{}'\nexit $code\n",
                script.display(),
                fixture_binary("fake_claude_script.py"),
                dir.join("exits.log").display(),
            ),
        );
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(&binary));
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
            dir.to_path_buf(),
            Some(config.clone()),
            events_notify,
        );
        engine.turn_gate().set_tick(StdDuration::from_millis(50));
        let dispatcher = dispatch.then(|| engine.start_dispatcher());
        Q {
            pool,
            engine,
            dir,
            config,
            flow,
            project,
            names: std::sync::Mutex::new(Vec::new()),
            dispatcher,
        }
    }

    /// Sets what each named turn does. A turn's name is the input its task is
    /// created with; none may contain another.
    fn script(&self, turns: &[(&str, Ending)]) {
        let mut by_prompt = serde_json::Map::new();
        let mut names = self.names.lock().unwrap();
        for (name, ending) in turns {
            by_prompt.insert((*name).to_string(), json!(held(&self.dir, name, *ending)));
            names.push((*name).to_string());
        }
        fs::write(
            self.dir.join("fake-claude-queue.json"),
            json!({ "by_prompt": by_prompt }).to_string(),
        )
        .unwrap();
    }

    async fn create_in(&self, project_id: &str, name: &str) -> Task {
        self.engine
            .create_task_from(
                project_id,
                WorkflowRef::File(self.flow.clone()),
                name,
                name,
                json!({}),
                None,
                &[],
            )
            .await
            .unwrap()
    }

    async fn create(&self, name: &str) -> Task {
        self.create_in(&self.project.id.clone(), name).await
    }

    async fn project(&self, name: &str, cap: Option<u32>) -> Project {
        projects::create_capped(&self.pool, name, None, cap)
            .await
            .unwrap()
    }

    fn started(&self, name: &str) -> bool {
        self.dir.join(format!("started-{name}")).exists()
    }

    async fn wait_started(&self, name: &str) {
        crate::test_support::wait_until(&format!("{name} to start"), || async {
            if self.started(name) {
                Ok(())
            } else {
                Err("not started".to_string())
            }
        })
        .await
    }

    /// The turn has not started, and (after a pause long enough for a wrong
    /// start to show) still has not.
    async fn assert_not_started(&self, name: &str) {
        tokio::time::sleep(StdDuration::from_millis(250)).await;
        assert!(!self.started(name), "{name} started but should be waiting");
    }

    fn release(&self, name: &str) {
        fs::write(self.dir.join(format!("release-{name}")), "").unwrap();
    }

    async fn rows(&self) -> Vec<QueueRow> {
        turn_queue::list(&self.pool).await.unwrap()
    }

    async fn wait_rows(&self, n: usize) -> Vec<QueueRow> {
        crate::test_support::wait_until(&format!("{n} queue row(s)"), || async {
            let rows = self.rows().await;
            if rows.len() == n {
                Ok(rows)
            } else {
                Err(format!("{} row(s)", rows.len()))
            }
        })
        .await
    }

    async fn wait_running(&self, n: usize) {
        crate::test_support::wait_until(&format!("{n} running turn(s)"), || async {
            let running = self.engine.running_total();
            if running == n {
                Ok(())
            } else {
                Err(format!("{running} running"))
            }
        })
        .await
    }

    async fn sessions(&self, task_id: &str) -> Vec<Session> {
        sessions::list_for_task(&self.pool, task_id).await.unwrap()
    }

    async fn events(&self, task_id: &str) -> Vec<chocofactory_core::models::Event> {
        events::list_for_task(&self.pool, task_id).await.unwrap()
    }

    async fn queue_events(&self, task_id: &str, kind: EventType) -> Vec<Value> {
        self.events(task_id)
            .await
            .into_iter()
            .filter(|e| e.event_type == kind)
            .map(|e| e.payload)
            .collect()
    }

    async fn status(&self, task_id: &str) -> String {
        tasks::get(&self.pool, task_id)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    async fn wait_status(&self, task_id: &str, status: &str) {
        wait_until_task_status(&self.pool, task_id, status).await
    }

    /// The task's position in line, from the snapshot the API serves.
    async fn position(&self, task_id: &str) -> Option<u32> {
        let snapshot = self.engine.queue_snapshot().await.unwrap();
        snapshot.queued_for(task_id).first().map(|q| q.position)
    }
}

// ---- the pure rules ------------------------------------------------------

#[test]
fn queue_row_fate_has_an_arm_for_every_status() {
    assert_eq!(queue_row_fate(None), RowFate::Withdraw);
    assert_eq!(queue_row_fate(Some("open")), RowFate::Admissible);
    assert_eq!(queue_row_fate(Some("stuck")), RowFate::Withdraw);
    assert_eq!(queue_row_fate(Some("closed")), RowFate::Withdraw);
    assert_eq!(queue_row_fate(Some("cancelled")), RowFate::Withdraw);
    assert_eq!(queue_row_fate(Some("paused")), RowFate::Keep);
}

#[tokio::test]
async fn the_sql_and_rust_admissible_predicates_agree_for_every_status() {
    let q = Q::new(None).await;
    for (i, status) in ["open", "stuck", "closed", "cancelled", "other"]
        .into_iter()
        .enumerate()
    {
        let task = tasks::create(
            &q.pool,
            tasks::NewTask {
                project_id: &q.project.id,
                workflow_def: "w",
                title: "T",
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
                base_ref: None,
                base_commit: None,
                closes: &[],
            },
        )
        .await
        .unwrap();
        tasks::update_status(&q.pool, &task.id, status)
            .await
            .unwrap();
        turn_queue::insert(
            &q.pool,
            turn_queue::NewQueueRow {
                task_id: &task.id,
                stage: "coding",
                group_stage: None,
                group_entry: None,
                resume_session_id: None,
            },
        )
        .await
        .unwrap();
        let row = q.rows().await.into_iter().last().unwrap();
        assert_eq!(row.admissible, is_admissible(status), "{status} ({i})");
    }
    assert_eq!(turn_queue::ADMISSIBLE_SQL, "t.status = 'open'");
}

// ---- test 1: no cap, nothing changes -------------------------------------

#[tokio::test]
async fn with_no_cap_every_turn_starts_at_once_and_writes_no_queue_state() {
    let q = Q::new(None).await;
    q.script(&[
        ("job-a", Ending::Report),
        ("job-b", Ending::Report),
        ("job-c", Ending::Report),
    ]);
    let mut ids = Vec::new();
    for name in ["job-a", "job-b", "job-c"] {
        ids.push(q.create(name).await.id);
    }
    for name in ["job-a", "job-b", "job-c"] {
        q.wait_started(name).await;
    }
    assert_eq!(q.engine.running_total(), 3);
    assert!(q.rows().await.is_empty());
    for id in &ids {
        for kind in [
            EventType::TurnQueued,
            EventType::TurnDequeued,
            EventType::TurnWithdrawn,
        ] {
            assert!(q.queue_events(id, kind).await.is_empty(), "{kind}");
        }
        assert!(q.sessions(id).await.len() == 1);
    }
}

#[tokio::test]
async fn with_no_cap_a_spawn_error_still_returns_from_create_and_queues_nothing() {
    let q = Q::new(None).await;
    // The fake agent is gone, so the spawn fails.
    fs::remove_file(q.dir.join("fake-claude-queue")).unwrap();
    let err = q
        .engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(q.flow.clone()),
            "t",
            "job-x",
            json!({}),
            None,
            &[],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::Start { .. }), "{err}");
    assert!(q.rows().await.is_empty());
    assert_eq!(q.engine.running_total(), 0);
}

// ---- test 2: cap 1, the second waits with no session or process ----------

#[tokio::test]
async fn the_second_turn_waits_with_no_session_and_starts_after_the_first_ended() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    assert_eq!(q.position(&b.id).await, Some(1));
    assert_eq!(q.status(&b.id).await, "open");
    let queued = q.queue_events(&b.id, EventType::TurnQueued).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0]["position"], 1);
    assert_eq!(queued[0]["limit"], 1);
    assert_eq!(queued[0]["pool"], "server");
    // No session row, no process.
    q.assert_not_started("job-b").await;
    assert!(q.sessions(&b.id).await.is_empty());
    assert_eq!(q.engine.running_total(), 1);

    q.release("job-a");
    q.wait_started("job-b").await;
    q.wait_status(&a.id, "closed").await;
    let second = q.sessions(&b.id).await.remove(0);
    // The first agent process was gone before the second session existed.
    let first_exit: f64 = fs::read_to_string(q.dir.join("exits.log"))
        .unwrap()
        .lines()
        .next()
        .expect("the first process logged its exit")
        .parse()
        .unwrap();
    let second_start = second.started_at.timestamp_micros() as f64 / 1e6;
    assert!(
        second_start >= first_exit,
        "the second session started at {second_start} before the first process ended at {first_exit}"
    );
    assert!(q.rows().await.is_empty());
    let dequeued = q.queue_events(&b.id, EventType::TurnDequeued).await;
    assert_eq!(dequeued.len(), 1);
    assert_eq!(dequeued[0]["session_id"], second.id);
}

// ---- test 3: no timer runs while queued ----------------------------------

#[tokio::test]
async fn a_queued_turn_gets_no_nudge_and_its_first_nudge_comes_after_it_starts() {
    let q = Q::new(Some(1)).await;
    // `job-b` ends its turn without reporting, so it is nudged after
    // `nudge_after` (200 ms here) once it runs.
    let mut by_prompt = serde_json::Map::new();
    by_prompt.insert("job-a".into(), json!(held(&q.dir, "job-a", Ending::Report)));
    by_prompt.insert(
        "job-b".into(),
        json!([
            {"op": "read_turn"},
            {"op": "run", "command": format!("touch '{}'", q.dir.join("started-job-b").display())},
            {"op": "result"},
            {"op": "answer_every_turn", "text": "still here"},
        ]),
    );
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!({ "by_prompt": by_prompt }).to_string(),
    )
    .unwrap();
    q.names.lock().unwrap().push("job-a".into());
    q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    // Several nudge periods and the whole grace pass while it waits.
    tokio::time::sleep(StdDuration::from_millis(900)).await;
    assert!(q.sessions(&b.id).await.is_empty());
    assert!(q.events(&b.id).await.iter().all(|e| e.session_id.is_none()));
    let released_at = chrono::Utc::now();
    q.release("job-a");
    q.wait_started("job-b").await;
    let nudge = crate::test_support::wait_until("the first nudge", || async {
        let notes: Vec<_> = q
            .events(&b.id)
            .await
            .into_iter()
            .filter(|e| e.event_type == EventType::SessionNote && e.payload["kind"] == "nudge")
            .collect();
        notes
            .into_iter()
            .next()
            .ok_or_else(|| "none yet".to_string())
    })
    .await;
    let session = q.sessions(&b.id).await.remove(0);
    assert!(session.started_at >= released_at);
    assert!(
        nudge.created_at - session.started_at >= chrono::Duration::milliseconds(150),
        "the nudge came {} after the start",
        nudge.created_at - session.started_at
    );
}

// ---- test 4: FIFO ---------------------------------------------------------

#[tokio::test]
async fn queued_turns_start_in_the_order_they_were_queued() {
    let q = Q::new(Some(1)).await;
    q.script(&[
        ("job-a", Ending::Report),
        ("job-b", Ending::Report),
        ("job-c", Ending::Report),
        ("job-d", Ending::Report),
    ]);
    let mut ids = Vec::new();
    for name in ["job-a", "job-b", "job-c", "job-d"] {
        ids.push(q.create(name).await.id);
        if name == "job-a" {
            q.wait_started(name).await;
        }
    }
    assert_eq!(q.position(&ids[1]).await, Some(1));
    assert_eq!(q.position(&ids[2]).await, Some(2));
    assert_eq!(q.position(&ids[3]).await, Some(3));
    for name in ["job-a", "job-b", "job-c", "job-d"] {
        q.wait_started(name).await;
        q.release(name);
    }
    let mut starts = Vec::new();
    for id in &ids {
        q.wait_status(id, "closed").await;
        starts.push(q.sessions(id).await.remove(0).started_at);
    }
    assert!(
        starts.windows(2).all(|w| w[0] <= w[1]),
        "turns started out of order: {starts:?}"
    );
}

// ---- test 5: pools --------------------------------------------------------

#[tokio::test]
async fn a_full_capped_project_does_not_hold_up_the_server_pool() {
    // No dispatcher: only the claim itself can start a turn.
    let q = Q::build(Some(1), queue_timers(), false).await;
    q.script(&[
        ("job-p1", Ending::Report),
        ("job-p2", Ending::Report),
        ("job-s1", Ending::Report),
    ]);
    let capped = q.project("capped", Some(1)).await;
    q.create_in(&capped.id, "job-p1").await;
    q.wait_started("job-p1").await;
    let p2 = q.create_in(&capped.id, "job-p2").await;
    assert_eq!(q.position(&p2.id).await, Some(1));
    // The server pool has its slot free: its turn starts in the call that
    // creates it, with no dispatcher to help and no queue row of its own.
    let s1 = q.create("job-s1").await;
    q.wait_started("job-s1").await;
    assert!(
        q.queue_events(&s1.id, EventType::TurnQueued)
            .await
            .is_empty()
    );
    q.assert_not_started("job-p2").await;
    assert_eq!(
        q.rows().await.len(),
        1,
        "only the capped project's p2 waits"
    );
}

#[tokio::test]
async fn a_project_caps_running_turns_do_not_count_toward_the_server_cap() {
    // No dispatcher: only the claim itself can start a turn.
    let q = Q::build(Some(1), queue_timers(), false).await;
    let capped = q.project("capped", Some(3)).await;
    let names = ["job-p1", "job-p2", "job-p3", "job-s1", "job-s2"];
    q.script(&names.map(|n| (n, Ending::Report)));
    for name in &names[..3] {
        q.create_in(&capped.id, name).await;
    }
    // A project cap of 3 under a server cap of 1 runs 3.
    for name in &names[..3] {
        q.wait_started(name).await;
    }
    q.wait_running(3).await;
    // The server pool still has its own single slot.
    let s1 = q.create("job-s1").await;
    q.wait_started("job-s1").await;
    assert!(
        q.queue_events(&s1.id, EventType::TurnQueued)
            .await
            .is_empty()
    );
    let s2 = q.create("job-s2").await;
    assert_eq!(q.position(&s2.id).await, Some(1));
    q.assert_not_started("job-s2").await;
}

#[tokio::test]
async fn pools_with_interleaved_turns_each_keep_their_own_order() {
    let q = Q::new(Some(1)).await;
    let capped = q.project("capped", Some(1)).await;
    let names = ["job-s1", "job-p1", "job-s2", "job-p2", "job-s3"];
    q.script(&names.map(|n| (n, Ending::Report)));
    let s1 = q.create("job-s1").await;
    let p1 = q.create_in(&capped.id, "job-p1").await;
    q.wait_started("job-s1").await;
    q.wait_started("job-p1").await;
    let s2 = q.create("job-s2").await;
    let p2 = q.create_in(&capped.id, "job-p2").await;
    let s3 = q.create("job-s3").await;
    // Positions are per pool, whatever the interleaving.
    assert_eq!(q.position(&s2.id).await, Some(1));
    assert_eq!(q.position(&p2.id).await, Some(1));
    assert_eq!(q.position(&s3.id).await, Some(2));
    // Freeing the capped project's slot starts its own head, not the older
    // server-pool row.
    q.release("job-p1");
    q.wait_started("job-p2").await;
    q.wait_status(&p1.id, "closed").await;
    q.assert_not_started("job-s2").await;
    assert_eq!(q.position(&s2.id).await, Some(1));
    // And the server pool drains in order.
    q.release("job-s1");
    q.wait_started("job-s2").await;
    q.assert_not_started("job-s3").await;
    q.release("job-s2");
    q.wait_started("job-s3").await;
    q.wait_status(&s1.id, "closed").await;
}

#[tokio::test]
async fn removing_a_project_cap_moves_its_queue_into_the_server_pool() {
    let q = Q::new(None).await;
    let capped = q.project("capped", Some(1)).await;
    q.script(&[("job-p1", Ending::Report), ("job-p2", Ending::Report)]);
    q.create_in(&capped.id, "job-p1").await;
    q.wait_started("job-p1").await;
    let p2 = q.create_in(&capped.id, "job-p2").await;
    assert_eq!(q.position(&p2.id).await, Some(1));
    projects::update_capped(&q.pool, &capped.id, None, None, Some(Some(0)))
        .await
        .unwrap();
    q.engine.wake_dispatcher();
    q.wait_started("job-p2").await;
}

// ---- test 6: config edits apply without a restart ------------------------

#[tokio::test]
async fn raising_max_agents_starts_queued_turns_and_lowering_it_kills_nothing() {
    let q = Q::new(Some(1)).await;
    q.script(&[
        ("job-a", Ending::Report),
        ("job-b", Ending::Report),
        ("job-c", Ending::Report),
    ]);
    q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    let c = q.create("job-c").await;
    q.assert_not_started("job-b").await;
    fs::write(&q.config, "max_agents: 3\n").unwrap();
    q.wait_started("job-b").await;
    q.wait_started("job-c").await;
    assert_eq!(q.engine.running_total(), 3);
    // Lowering it stops nothing.
    fs::write(&q.config, "max_agents: 1\n").unwrap();
    tokio::time::sleep(StdDuration::from_millis(250)).await;
    assert_eq!(q.engine.running_total(), 3);
    assert_eq!(q.status(&b.id).await, "open");
    assert_eq!(q.status(&c.id).await, "open");
}

// ---- test 7: every way a turn ends frees its slot exactly once -----------

async fn ending_frees_the_slot(ending: Ending, first_status: &str) {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", ending), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    assert_eq!(q.position(&b.id).await, Some(1));
    assert_eq!(q.engine.running_total(), 1);
    q.release("job-a");
    // The next queued turn starts, and the running count never exceeds 1.
    q.wait_started("job-b").await;
    q.wait_status(&a.id, first_status).await;
    q.wait_running(1).await;
    q.release("job-b");
    q.wait_status(&b.id, "closed").await;
    // Exactly once: the count is back to zero, not below it, and a new
    // turn starts at once.
    q.wait_running(0).await;
    q.script(&[("job-z", Ending::Report)]);
    q.create("job-z").await;
    q.wait_started("job-z").await;
}

#[tokio::test]
async fn a_turn_that_reports_frees_its_slot() {
    ending_frees_the_slot(Ending::Report, "closed").await;
}

#[tokio::test]
async fn a_turn_that_crashes_frees_its_slot() {
    ending_frees_the_slot(Ending::Crash, "stuck").await;
}

#[tokio::test]
async fn a_turn_that_never_reports_frees_its_slot() {
    ending_frees_the_slot(Ending::NoReport, "stuck").await;
}

#[tokio::test]
async fn a_turn_that_hits_a_usage_limit_frees_its_slot() {
    ending_frees_the_slot(Ending::UsageLimit, "stuck").await;
}

#[tokio::test]
async fn a_turn_whose_process_lingers_frees_its_slot() {
    ending_frees_the_slot(Ending::Lingers, "stuck").await;
}

#[tokio::test]
async fn cancelling_a_running_turn_frees_its_slot() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.create("job-b").await;
    q.engine.cancel_task(&a.id, false).await.unwrap();
    q.wait_started("job-b").await;
    q.wait_running(1).await;
}

#[tokio::test]
async fn a_turn_that_fails_to_spawn_frees_its_slot() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    // The agent binary disappears, so the queued turn's spawn fails when it
    // is admitted.
    fs::remove_file(q.dir.join("fake-claude-queue")).unwrap();
    q.release("job-a");
    q.wait_status(&a.id, "closed").await;
    q.wait_status(&b.id, "stuck").await;
    let task = tasks::get(&q.pool, &b.id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .unwrap()
            .contains("after waiting in the queue"),
        "the reason says the turn had queued"
    );
    q.wait_running(0).await;
    assert!(q.rows().await.is_empty());
    assert!(
        q.sessions(&b.id)
            .await
            .iter()
            .all(|s| s.end_reason == Some(SessionEndReason::StartFailed))
    );
}

// ---- abandoned operations --------------------------------------------------

#[tokio::test]
async fn a_claim_whose_future_is_dropped_before_the_spawn_frees_its_slot() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-d", Ending::Report)]);
    let c = q.create_in_without_start("job-c").await;
    let gate = Arc::clone(q.engine.turn_gate());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task_id = c.id.clone();
    // A claim that returns `Start(permit)`, in a future aborted before any
    // `launch_turn` could spawn: the permit goes with the future.
    let handle = tokio::spawn(async move {
        let spec = TurnSpec {
            stage: "coding",
            group: None,
            resume_session_id: None,
        };
        let claim = gate.claim(&task_id, &spec).await.unwrap();
        assert!(matches!(claim, Claim::Start(_)));
        tx.send(()).unwrap();
        let _held = claim;
        std::future::pending::<()>().await;
    });
    rx.await.unwrap();
    assert_eq!(q.engine.running_total(), 1);
    // The next turn has to wait for it.
    let d = q.create("job-d").await;
    assert_eq!(q.position(&d.id).await, Some(1));
    handle.abort();
    let _ = handle.await;
    // The running count is back to its earlier value (0) and the queued turn
    // took the freed slot.
    q.wait_started("job-d").await;
    assert_eq!(q.engine.running_total(), 1);
    assert!(q.rows().await.is_empty());
}

#[tokio::test]
async fn aborting_the_turn_watcher_frees_its_slot_and_the_next_turn_starts() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-b", Ending::Report)]);
    // A turn whose watcher holds the only permit. The session row stays
    // `active` (no process), so the watcher keeps waiting until aborted.
    let x = q.create_in_without_start("job-x").await;
    let session = sessions::create(
        &q.pool,
        sessions::NewSession {
            task_id: &x.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
    )
    .await
    .unwrap();
    let spec = TurnSpec {
        stage: "coding",
        group: None,
        resume_session_id: None,
    };
    let Claim::Start(permit) = q.engine.turn_gate().claim(&x.id, &spec).await.unwrap() else {
        panic!("the slot was free");
    };
    let watcher = q.engine.spawn_turn_watcher(
        x.id.clone(),
        Arc::new(WorkflowDefinition::parse(FLOW, &q.dir).unwrap()),
        "coding".to_string(),
        None,
        session.id.clone(),
        None,
        Some(permit),
    );
    assert_eq!(q.engine.running_total(), 1);
    let b = q.create("job-b").await;
    assert_eq!(q.position(&b.id).await, Some(1));
    q.assert_not_started("job-b").await;
    watcher.abort();
    let _ = watcher.await;
    q.wait_started("job-b").await;
    // Exactly one slot is held again: the new turn's.
    assert_eq!(q.engine.running_total(), 1);
}

impl Q {
    /// A task row with its workflow state, but no turn started.
    async fn create_in_without_start(&self, name: &str) -> Task {
        let def = Arc::new(WorkflowDefinition::parse(FLOW, &self.dir).unwrap());
        tasks::create(
            &self.pool,
            tasks::NewTask {
                project_id: &self.project.id,
                workflow_def: &def.name,
                title: name,
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
                base_ref: None,
                base_commit: None,
                closes: &[],
            },
        )
        .await
        .unwrap()
    }
}

// ---- more helpers ----------------------------------------------------------

impl Q {
    /// Sets what each branch of the group workflow does: `(marker, name,
    /// ending)` where the marker is the branch's prompt text.
    fn script_branches(&self, branches: &[(&str, &str, Ending)]) {
        let mut by_prompt = serde_json::Map::new();
        let mut names = self.names.lock().unwrap();
        for (marker, name, ending) in branches {
            by_prompt.insert((*marker).to_string(), json!(held(&self.dir, name, *ending)));
            names.push((*name).to_string());
        }
        fs::write(
            self.dir.join("fake-claude-queue.json"),
            json!({ "by_prompt": by_prompt }).to_string(),
        )
        .unwrap();
    }

    async fn create_group(&self) -> Task {
        // Branches are read-only, so their turns snapshot a git worktree.
        let repo = self.dir.join("repo");
        if !repo.exists() {
            fs::create_dir_all(&repo).unwrap();
            init_git_repo(&repo).await;
        }
        self.engine
            .create_task_from(
                &self.project.id,
                WorkflowRef::File(self.dir.join("qgroup.yaml")),
                "panel",
                "go",
                json!({ "cwd": repo.to_string_lossy() }),
                None,
                &[],
            )
            .await
            .unwrap()
    }

    /// A second daemon on the same database: what a restart builds.
    fn restarted_engine(&self) -> Arc<WorkflowEngine> {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(ClaudeAdapter::with_binary(
            self.dir.join("fake-claude-queue").display().to_string(),
        ));
        let events_notify = Arc::new(Notify::new());
        let manager = SessionManager::with_turn_timers(
            self.pool.clone(),
            Registry::single(adapter),
            chrono::Duration::hours(1),
            Arc::clone(&events_notify),
            queue_timers(),
        );
        let engine = WorkflowEngine::new(
            self.pool.clone(),
            manager,
            self.dir.to_path_buf(),
            Some(self.config.clone()),
            events_notify,
        );
        engine.turn_gate().set_tick(StdDuration::from_millis(50));
        engine
    }

    /// The old daemon dies, and the new one does what `main` does at
    /// startup, before serving.
    async fn crash_and_sweep(&self, engine: &Arc<WorkflowEngine>) {
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.abort();
        }
        self.engine.simulate_crash();
        Q::sweep(engine).await;
    }

    async fn sweep(engine: &Arc<WorkflowEngine>) {
        sessions::recover_stale_active_sessions(&engine.pool)
            .await
            .unwrap();
        engine.park_interrupted_turns().await.unwrap();
        engine.resume_interrupted_polls().await.unwrap();
    }

    async fn task_row(&self, name: &str) -> Task {
        self.create_in_without_start(name).await
    }

    /// An open task parked at `coding` with a workflow state, as if its turn
    /// had been entered.
    async fn task_at(&self, stage: &str, status: &str) -> Task {
        self.task_with(stage, status, &self.flow.display().to_string(), json!({}))
            .await
    }

    /// [`Self::task_at`] with the workflow file and the task config chosen.
    async fn task_with(&self, stage: &str, status: &str, workflow: &str, config: Value) -> Task {
        let task = tasks::create(
            &self.pool,
            tasks::NewTask {
                project_id: &self.project.id,
                workflow_def: "qflow",
                title: "row",
                config,
                workflow_path: Some(workflow),
                workflow_sha256: Some("0"),
                base_ref: None,
                base_commit: None,
                closes: &[],
            },
        )
        .await
        .unwrap();
        workflow_state::create(
            &self.pool,
            &task.id,
            stage,
            if stage == "coding" {
                "agent_turn"
            } else {
                "terminal"
            },
            json!({ "task": { "input": "job-r", "title": "row" } }),
        )
        .await
        .unwrap();
        if status != "open" {
            tasks::update_status(&self.pool, &task.id, status)
                .await
                .unwrap();
        }
        task
    }

    async fn insert_row(&self, task_id: &str, stage: &str) -> i64 {
        turn_queue::insert(
            &self.pool,
            turn_queue::NewQueueRow {
                task_id,
                stage,
                group_stage: None,
                group_entry: None,
                resume_session_id: None,
            },
        )
        .await
        .unwrap()
    }
}

// ---- test 9: cancel -----------------------------------------------------

#[tokio::test]
async fn cancelling_a_queued_task_removes_its_row_and_nothing_ever_starts_it() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    assert_eq!(q.rows().await.len(), 1);
    q.engine.cancel_task(&b.id, false).await.unwrap();
    assert!(q.rows().await.is_empty());
    assert_eq!(q.status(&b.id).await, "cancelled");
    let withdrawn = q.queue_events(&b.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0]["reason"], "cancel");
    q.release("job-a");
    q.wait_running(0).await;
    q.assert_not_started("job-b").await;
    assert!(q.sessions(&b.id).await.is_empty());
}

#[tokio::test]
async fn cancel_and_the_queue_delete_are_one_transaction() {
    let q = Q::new(None).await;
    let task = q.task_at("coding", "open").await;
    q.insert_row(&task.id, "coding").await;
    let (cancelled, withdrawn) = tasks::mark_cancelled(&q.pool, &task.id, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(withdrawn, vec![("coding".to_string(), None)]);
    assert!(q.rows().await.is_empty());
    // A task that does not exist changes nothing and deletes nothing.
    let other = q.task_at("coding", "open").await;
    q.insert_row(&other.id, "coding").await;
    assert!(
        tasks::mark_cancelled(&q.pool, "no-such-task", false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(q.rows().await.len(), 1);
}

#[tokio::test]
async fn deleting_a_task_deletes_its_queue_rows_first() {
    let q = Q::new(None).await;
    let task = q.task_row("job-d").await;
    q.insert_row(&task.id, "coding").await;
    assert!(tasks::delete(&q.pool, &task.id).await.unwrap());
    assert!(q.rows().await.is_empty());
}

// ---- test 13: the dispatcher withdraws rows that can never start ----------

#[tokio::test]
async fn the_dispatcher_withdraws_rows_of_tasks_that_can_never_start_and_starts_nothing() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-n", Ending::Report)]);
    let mut gone = Vec::new();
    for status in ["stuck", "closed", "cancelled"] {
        let task = q.task_at("coding", status).await;
        q.insert_row(&task.id, "coding").await;
        gone.push((task.id, status));
    }
    q.wait_rows(0).await;
    for (id, status) in &gone {
        let withdrawn = q.queue_events(id, EventType::TurnWithdrawn).await;
        assert_eq!(withdrawn.len(), 1, "{status}");
        assert_eq!(withdrawn[0]["reason"], format!("the task is {status}"));
        assert!(q.sessions(id).await.is_empty());
    }
    // The slot is free: the next turn starts at once.
    q.create("job-n").await;
    q.wait_started("job-n").await;
}

#[tokio::test]
async fn the_dispatcher_withdraws_a_row_whose_task_moved_on_and_frees_the_slot() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-n", Ending::Report)]);
    // Open, but sitting in a different stage than the row names: a
    // validation refusal.
    let task = q.task_at("finished", "open").await;
    q.insert_row(&task.id, "coding").await;
    q.wait_rows(0).await;
    let withdrawn = q.queue_events(&task.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn.len(), 1);
    assert!(
        withdrawn[0]["reason"]
            .as_str()
            .unwrap()
            .contains("moved on"),
        "{withdrawn:?}"
    );
    assert!(q.sessions(&task.id).await.is_empty());
    assert_eq!(q.status(&task.id).await, "open");
    q.wait_running(0).await;
    q.create("job-n").await;
    q.wait_started("job-n").await;
}

#[tokio::test]
async fn a_row_of_a_task_with_an_unknown_status_is_kept() {
    let q = Q::new(Some(1)).await;
    let task = q.task_at("coding", "future_status").await;
    q.insert_row(&task.id, "coding").await;
    tokio::time::sleep(StdDuration::from_millis(400)).await;
    assert_eq!(q.rows().await.len(), 1);
    assert!(
        q.queue_events(&task.id, EventType::TurnWithdrawn)
            .await
            .is_empty()
    );
    assert!(q.sessions(&task.id).await.is_empty());
}

// ---- retry withdraws leftover rows ----------------------------------------

/// A stuck task with a leftover queue row, as a failed step-5 delete leaves.
async fn stuck_with_leftover_row(q: &Q) -> Task {
    q.script(&[("job-r", Ending::Crash)]);
    let task = q.create("job-r").await;
    q.wait_started("job-r").await;
    q.release("job-r");
    q.wait_status(&task.id, "stuck").await;
    q.wait_running(0).await;
    q.insert_row(&task.id, "coding").await;
    // The retried turn runs to completion.
    fs::remove_file(q.dir.join("release-job-r")).unwrap();
    fs::remove_file(q.dir.join("started-job-r")).unwrap();
    q.script(&[("job-r", Ending::Report)]);
    task
}

#[tokio::test]
async fn a_leftover_row_is_withdrawn_on_the_next_pass_and_a_retry_then_claims_normally() {
    let q = Q::new(Some(1)).await;
    let task = stuck_with_leftover_row(&q).await;
    q.wait_rows(0).await;
    let withdrawn = q.queue_events(&task.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0]["reason"], "the task is stuck");
    let outcome = q
        .engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap();
    assert_eq!(outcome.queued, None);
    q.wait_started("job-r").await;
    q.release("job-r");
    q.wait_status(&task.id, "closed").await;
}

#[tokio::test]
async fn a_retry_before_any_dispatcher_pass_withdraws_the_leftover_row_without_a_unique_error() {
    // No dispatcher at all: only the retry can clear the row.
    let q = Q::build(Some(1), queue_timers(), false).await;
    let task = stuck_with_leftover_row(&q).await;
    assert_eq!(q.rows().await.len(), 1);
    let outcome = q
        .engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap();
    assert_eq!(outcome.queued, None, "the freed slot is claimed normally");
    q.wait_started("job-r").await;
    assert!(q.rows().await.is_empty());
    let withdrawn = q.queue_events(&task.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0]["reason"], "retry");
    q.release("job-r");
    q.wait_status(&task.id, "closed").await;
}

#[tokio::test]
async fn retrying_into_a_full_pool_queues_the_turn_and_reports_its_place() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Crash), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.release("job-a");
    q.wait_status(&a.id, "stuck").await;
    q.wait_running(0).await;
    // Another task now holds the only slot.
    q.create("job-b").await;
    q.wait_started("job-b").await;
    let outcome = q.engine.retry_task(&a.id, RetryMode::Fresh).await.unwrap();
    assert_eq!(outcome.queued, Some(1));
    assert_eq!(q.status(&a.id).await, "open");
    assert_eq!(q.rows().await.len(), 1);
    // A queued task is open, so retrying it again is refused, and the
    // refusal names its place in line.
    let err = q
        .engine
        .retry_task(&a.id, RetryMode::Fresh)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RetryTaskError::Queued { position: 1, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("1st in line"), "{err}");
}

// ---- test 11: a queued retry --resume resumes the recorded session --------

#[tokio::test]
async fn a_queued_retry_resume_resumes_the_session_it_recorded() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::UsageLimit), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    q.release("job-a");
    q.wait_status(&a.id, "stuck").await;
    q.wait_running(0).await;
    let old = q.sessions(&a.id).await.remove(0);
    q.create("job-b").await;
    q.wait_started("job-b").await;
    // Queued: the row remembers the session to resume.
    let outcome = q.engine.retry_task(&a.id, RetryMode::Resume).await.unwrap();
    assert!(outcome.resumed);
    assert_eq!(outcome.queued, Some(1));
    let rows = q.rows().await;
    assert_eq!(rows[0].resume_session_id.as_deref(), Some(old.id.as_str()));
    // Once admitted, the new session continues the old one.
    fs::remove_file(q.dir.join("started-job-a")).ok();
    // The resumed turn is told it was interrupted (not its old prompt), so
    // it follows a plain script.
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!(held(&q.dir, "job-a", Ending::Report)).to_string(),
    )
    .unwrap();
    q.release("job-b");
    q.wait_status(&a.id, "closed").await;
    let sessions = q.sessions(&a.id).await;
    assert_eq!(sessions.len(), 2);
    let new = sessions.iter().find(|s| s.id != old.id).unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some(old.id.as_str()));
}

// ---- fail-closed config ---------------------------------------------------

#[tokio::test]
async fn claim_fails_when_the_config_is_malformed_and_names_the_file() {
    let q = Q::new(Some(1)).await;
    fs::write(&q.config, "max_agents: two\n").unwrap();
    let task = q.task_row("job-c").await;
    let spec = TurnSpec {
        stage: "coding",
        group: None,
        resume_session_id: None,
    };
    let err = q
        .engine
        .turn_gate()
        .claim(&task.id, &spec)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::GlobalConfig(_)), "{err}");
    assert!(
        err.to_string().contains(&q.config.display().to_string()),
        "{err}"
    );
    assert!(q.rows().await.is_empty());
    assert_eq!(q.engine.running_total(), 0);
}

async fn a_broken_config_parks_the_new_turn_stuck(break_config: impl FnOnce(&Path)) {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report)]);
    fs::remove_file(&q.config).unwrap();
    break_config(&q.config);
    let err = q
        .engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(q.flow.clone()),
            "t",
            "job-a",
            json!({}),
            None,
            &[],
        )
        .await
        .unwrap_err();
    let CreateTaskError::Start { task_id, .. } = err else {
        panic!("expected a start failure: {err}");
    };
    let task = tasks::get(&q.pool, &task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains(&q.config.display().to_string()),
        "the stuck reason names the config file: {reason}"
    );
    assert!(q.rows().await.is_empty());
    assert!(q.sessions(&task_id).await.is_empty());
}

#[tokio::test]
async fn a_malformed_config_parks_the_new_turn_stuck_naming_the_file() {
    a_broken_config_parks_the_new_turn_stuck(|path| {
        fs::write(path, "max_agents: two\n").unwrap();
    })
    .await;
}

#[tokio::test]
async fn an_unreadable_config_parks_the_new_turn_stuck_naming_the_file() {
    // A directory where the file should be makes the read fail without chmod.
    a_broken_config_parks_the_new_turn_stuck(|path| fs::create_dir(path).unwrap()).await;
}

#[tokio::test]
async fn a_dispatcher_that_cannot_read_the_config_starts_nothing_reports_it_and_recovers() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    fs::write(&q.config, "max_agents: two\n").unwrap();
    q.release("job-a");
    q.wait_status(&a.id, "closed").await;
    let error = crate::test_support::wait_until("the queue error", || async {
        match q.engine.queue_snapshot().await.unwrap().queue_error {
            Some(error) => Ok(error),
            None => Err("no error yet".to_string()),
        }
    })
    .await;
    assert!(error.contains(&q.config.display().to_string()), "{error}");
    assert_eq!(
        q.engine.turn_gate().queue_error().as_deref(),
        Some(error.as_str())
    );
    // Nothing starts, and nothing is failed because of the bad edit.
    q.assert_not_started("job-b").await;
    assert_eq!(q.rows().await.len(), 1);
    assert_eq!(q.status(&b.id).await, "open");
    // Fixed: the queue drains.
    fs::write(&q.config, "max_agents: 1\n").unwrap();
    q.wait_started("job-b").await;
    assert_eq!(q.engine.queue_snapshot().await.unwrap().queue_error, None);
}

// ---- failed queue insert and vanished row --------------------------------

#[tokio::test]
async fn a_failed_queue_insert_is_an_error_and_the_start_fails() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-c", Ending::Report)]);
    q.create("job-a").await;
    q.wait_started("job-a").await;
    // The pool is full, so the next turn queues; a row for the same stage
    // already exists, so the insert violates UNIQUE (task_id, stage).
    let task = q.task_row("job-c").await;
    q.insert_row(&task.id, "coding").await;
    let def = Arc::new(WorkflowDefinition::parse(FLOW, &q.dir).unwrap());
    let err = q
        .engine
        .start_task(&task.id, &def, Some("job-c"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Db(e) if e.to_string().to_lowercase().contains("unique")),
        "{err}"
    );
    assert_eq!(q.rows().await.len(), 1, "no second row");
}

#[tokio::test]
async fn a_dequeue_whose_row_vanished_rolls_back_and_creates_no_session() {
    let q = Q::new(None).await;
    let task = q.task_row("job-v").await;
    let row = q.insert_row(&task.id, "coding").await;
    // The row is withdrawn by someone else before the session is created.
    assert!(turn_queue::delete(&q.pool, row).await.unwrap());
    let err = sessions::create_from_queue(
        &q.pool,
        row,
        sessions::NewSession {
            task_id: &task.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, sessions::CreateFromQueueError::Vanished),
        "{err}"
    );
    assert!(q.sessions(&task.id).await.is_empty());
    assert_eq!(q.engine.running_total(), 0);
    assert!(!q.started("job-v"));
}

#[tokio::test]
async fn a_dequeue_deletes_the_row_and_creates_the_session_together() {
    let q = Q::new(None).await;
    let task = q.task_row("job-v").await;
    let row = q.insert_row(&task.id, "coding").await;
    let session = sessions::create_from_queue(
        &q.pool,
        row,
        sessions::NewSession {
            task_id: &task.id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(session.task_id, task.id);
    assert!(q.rows().await.is_empty());
    assert_eq!(q.sessions(&task.id).await.len(), 1);
}

// ---- test 15: while stopping ----------------------------------------------

#[tokio::test]
async fn while_stopping_a_turn_queues_even_with_no_cap_and_starts_after_the_restart() {
    let q = Q::new(None).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    q.create("job-a").await;
    q.wait_started("job-a").await;
    q.engine.turn_gate().stop();
    let b = q.create("job-b").await;
    // Queued, with no limit anywhere: D9 beats D8.
    let rows = q.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task_id, b.id);
    let queued = q.queue_events(&b.id, EventType::TurnQueued).await;
    assert_eq!(queued[0]["limit"], Value::Null);
    // Even with a free slot (the first turn ends), nothing is admitted.
    q.release("job-a");
    q.wait_running(0).await;
    q.assert_not_started("job-b").await;
    assert_eq!(q.status(&b.id).await, "open");
    // After the restart it starts.
    let next = q.restarted_engine();
    Q::sweep(&next).await;
    let _dispatcher = next.start_dispatcher();
    q.wait_started("job-b").await;
    assert!(q.rows().await.is_empty());
}

// ---- test 8: restart ------------------------------------------------------

#[tokio::test]
async fn after_a_restart_queued_turns_are_still_queued_in_order_and_running_ones_are_stuck() {
    let q = Q::new(Some(1)).await;
    q.script(&[
        ("job-a", Ending::Report),
        ("job-b", Ending::Report),
        ("job-c", Ending::Report),
    ]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    let c = q.create("job-c").await;
    let before: Vec<i64> = q.rows().await.iter().map(|r| r.id).collect();
    // The old daemon dies; the new one sweeps.
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    assert_eq!(q.status(&a.id).await, "stuck", "the running turn is parked");
    assert_eq!(q.status(&b.id).await, "open");
    assert_eq!(q.status(&c.id).await, "open");
    let after: Vec<i64> = q.rows().await.iter().map(|r| r.id).collect();
    assert_eq!(before, after, "same rows, same order");
    assert!(q.sessions(&b.id).await.is_empty());
    // The queue drains in its old order.
    let _dispatcher = next.start_dispatcher();
    q.wait_started("job-b").await;
    q.assert_not_started("job-c").await;
    q.release("job-b");
    q.wait_started("job-c").await;
}

// ---- test 10: parallel groups ---------------------------------------------

#[tokio::test]
async fn a_group_whose_branches_do_not_all_fit_queues_the_rest_and_settles_after_they_run() {
    let q = Q::new(Some(2)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    q.assert_not_started("three").await;
    // Two run and one is queued: in the snapshot, the branch table and the
    // list, but not in the payload (a queued branch is "not settled").
    let snapshot = q.engine.queue_snapshot().await.unwrap();
    let queued = snapshot.queued_for(&g.id);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].stage, "three");
    assert_eq!(queued[0].group.as_deref(), Some("panel"));
    assert_eq!(queued[0].position, 1);
    let state = workflow_state::get(&q.pool, &g.id).await.unwrap().unwrap();
    let branches = q.engine.branch_statuses(&g, &state).await;
    let states: Vec<(&str, &str)> = branches
        .iter()
        .map(|b| (b.name.as_str(), b.state.as_str()))
        .collect();
    assert_eq!(
        states,
        vec![("one", "running"), ("two", "running"), ("three", "queued")]
    );
    assert_eq!(branches[2].queued_position, Some(1));
    assert_eq!(
        state.payload["parallel"]["branches"]["three"]["state"],
        "running"
    );
    assert!(runs_for_stage(&q.pool, &g.id, "three").await.is_empty());
    // The group is not settled while a branch is queued.
    q.release("one");
    q.wait_started("three").await;
    assert_eq!(q.status(&g.id).await, "open");
    q.release("two");
    q.release("three");
    q.wait_status(&g.id, "closed").await;
}

#[tokio::test]
async fn retrying_a_failed_branch_claims_a_slot_again() {
    let q = Q::new(Some(2)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Crash),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    q.release("two");
    q.wait_started("three").await;
    q.release("one");
    q.release("three");
    q.wait_status(&g.id, "stuck").await;
    q.wait_running(0).await;
    // Two other tasks fill both slots, so the retried branch has to queue.
    q.script_branches(&[("BRANCH-TWO", "two", Ending::Report)]);
    let mut by_prompt = serde_json::Map::new();
    for name in ["job-s1", "job-s2"] {
        by_prompt.insert(name.into(), json!(held(&q.dir, name, Ending::Report)));
        q.names.lock().unwrap().push(name.into());
    }
    by_prompt.insert(
        "BRANCH-TWO".into(),
        json!(held(&q.dir, "two", Ending::Report)),
    );
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!({ "by_prompt": by_prompt }).to_string(),
    )
    .unwrap();
    fs::remove_file(q.dir.join("release-two")).unwrap();
    q.create("job-s1").await;
    q.create("job-s2").await;
    q.wait_started("job-s1").await;
    q.wait_started("job-s2").await;
    let outcome = q.engine.retry_task(&g.id, RetryMode::Fresh).await.unwrap();
    assert_eq!(outcome.branches.len(), 1);
    assert_eq!(outcome.branches[0].branch, "two");
    assert_eq!(outcome.branches[0].queued, Some(1));
    q.release("job-s1");
    // The slot goes to the retried branch.
    crate::test_support::wait_until("the retried branch to run", || async {
        if q.engine
            .queue_snapshot()
            .await
            .unwrap()
            .queued_for(&g.id)
            .is_empty()
        {
            Ok(())
        } else {
            Err("still queued".to_string())
        }
    })
    .await;
    q.release("two");
    q.wait_status(&g.id, "closed").await;
}

#[tokio::test]
async fn a_queued_branch_survives_a_restart_while_its_running_siblings_are_parked() {
    let q = Q::new(Some(2)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    q.wait_started("two").await;
    let next = q.restarted_engine();
    q.crash_and_sweep(&next).await;
    // The queued branch is neither failed nor parked; the group is still
    // waiting on it.
    assert_eq!(q.status(&g.id).await, "open");
    let state = workflow_state::get(&q.pool, &g.id).await.unwrap().unwrap();
    assert_eq!(
        state.payload["parallel"]["branches"]["one"]["state"],
        "failed"
    );
    assert_eq!(
        state.payload["parallel"]["branches"]["two"]["state"],
        "failed"
    );
    assert_eq!(
        state.payload["parallel"]["branches"]["three"]["state"],
        "running"
    );
    assert_eq!(q.rows().await.len(), 1);
    let _dispatcher = next.start_dispatcher();
    q.wait_started("three").await;
    q.release("three");
    // With two failed branches, the group settles stuck once three ends.
    q.wait_status(&g.id, "stuck").await;
}

// ---- test 12: queued turns are not in flight -------------------------------

#[tokio::test]
async fn queued_turns_are_not_in_flight_but_running_ones_are() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-b", Ending::Report)]);
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let b = q.create("job-b").await;
    let in_flight = q.engine.in_flight().await.unwrap();
    let ids: Vec<&str> = in_flight.iter().map(|f| f.task_id.as_str()).collect();
    assert_eq!(ids, vec![a.id.as_str()]);
    let snapshot = q.engine.queue_snapshot().await.unwrap();
    assert_eq!(snapshot.turns.len(), 1);
    assert_eq!(snapshot.turns[0].task_id, b.id);
    assert_eq!(snapshot.turns[0].turn.position, 1);
    assert_eq!(snapshot.turns[0].turn.pool, "server");
    assert_eq!(snapshot.pools[0].running, 1);
    assert_eq!(snapshot.pools[0].queued, 1);
    assert_eq!(snapshot.pools[0].limit, Some(1));
}

#[tokio::test]
async fn a_group_whose_branches_are_all_queued_is_not_in_flight() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report)]);
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    // `job-a` is scripted by the first call; rebuild the combined script.
    let mut by_prompt = serde_json::Map::new();
    by_prompt.insert("job-a".into(), json!(held(&q.dir, "job-a", Ending::Report)));
    for (marker, name) in [
        ("BRANCH-ONE", "one"),
        ("BRANCH-TWO", "two"),
        ("BRANCH-THREE", "three"),
    ] {
        by_prompt.insert(marker.into(), json!(held(&q.dir, name, Ending::Report)));
    }
    fs::write(
        q.dir.join("fake-claude-queue.json"),
        json!({ "by_prompt": by_prompt }).to_string(),
    )
    .unwrap();
    q.names.lock().unwrap().push("job-a".into());
    let a = q.create("job-a").await;
    q.wait_started("job-a").await;
    let g = q.create_group().await;
    assert_eq!(q.rows().await.len(), 3);
    let ids: Vec<String> = q
        .engine
        .in_flight()
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.task_id)
        .collect();
    assert_eq!(
        ids,
        vec![a.id.clone()],
        "the all-queued group is not in flight"
    );
    // Once one branch runs, the group is.
    q.release("job-a");
    q.wait_started("one").await;
    let ids: Vec<String> = q
        .engine
        .in_flight()
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.task_id)
        .collect();
    assert_eq!(ids, vec![g.id.clone()]);
}

// ---- test 16: chat, shell and poll never claim ----------------------------

const MIXED_FLOW: &str = "name: qmixed\nworktree: false\nroles:\n  chat:\n    cli: claude\n    model: sonnet\nstages:\n  prep:\n    kind: shell\n    command: \"true\"\n    on: { done: watch }\n  watch:\n    kind: poll\n    command: \"echo SUCCESS\"\n    interval: 1s\n    outcomes:\n      - match: \"SUCCESS\"\n        then: green\n    on: { green: chatting }\n  chatting:\n    kind: agent_turn\n    role: chat\n    prompt_file: chat.md\n    on: {}\n";

#[tokio::test]
async fn chat_shell_and_poll_stages_never_wait_for_a_slot() {
    let q = Q::new(Some(1)).await;
    q.script(&[("job-a", Ending::Report), ("job-chat", Ending::Report)]);
    let mixed = q.dir.join("qmixed.yaml");
    fs::write(&mixed, MIXED_FLOW).unwrap();
    fs::write(q.dir.join("chat.md"), "job-chat").unwrap();
    q.create("job-a").await;
    q.wait_started("job-a").await;
    // The only slot is busy. The shell stage runs, the poll runs, and the
    // chat session starts, all without queueing.
    let task = q
        .engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(mixed),
            "mixed",
            "job-chat",
            json!({}),
            None,
            &[],
        )
        .await
        .unwrap();
    q.wait_started("job-chat").await;
    assert!(q.rows().await.is_empty());
    assert!(
        q.queue_events(&task.id, EventType::TurnQueued)
            .await
            .is_empty()
    );
    // Only the coder holds a slot; the chat session holds none.
    assert_eq!(q.engine.running_total(), 1);
    assert_eq!(q.engine.turn_gate().running_for_task(&task.id), 0);
}

// ---- the stress test --------------------------------------------------------

/// A seeded xorshift, so a failing run can be reproduced.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_tasks_claiming_at_once_under_cap_three_never_exceed_it_and_all_start() {
    const TASKS: usize = 20;
    let q = Arc::new(Q::new(Some(3)).await);
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    let endings = [
        Ending::Report,
        Ending::Crash,
        Ending::NoReport,
        Ending::UsageLimit,
        Ending::Lingers,
    ];
    let names: Vec<String> = (0..TASKS).map(|i| format!("job-t{i:02}")).collect();
    let plan: Vec<(&str, Ending)> = names
        .iter()
        .map(|n| {
            (
                n.as_str(),
                endings[rng.below(endings.len() as u64) as usize],
            )
        })
        .collect();
    q.script(&plan);

    // Sample the invariants for as long as the run lasts.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (q, stop) = (Arc::clone(&q), Arc::clone(&stop));
        tokio::spawn(async move {
            let mut samples = 0u32;
            while !stop.load(Ordering::SeqCst) {
                let running = q.engine.running_total();
                assert!(running <= 3, "{running} turns running under a cap of 3");
                let snapshot = q.engine.queue_snapshot().await.unwrap();
                let positions: Vec<u32> = snapshot.turns.iter().map(|t| t.turn.position).collect();
                let mut sorted = positions.clone();
                sorted.sort_unstable();
                let want: Vec<u32> = (1..=positions.len() as u32).collect();
                assert_eq!(sorted, want, "positions have a gap: {positions:?}");
                samples += 1;
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
            samples
        })
    };

    let mut creators = Vec::new();
    for name in &names {
        let (q, name) = (Arc::clone(&q), name.clone());
        creators.push(tokio::spawn(async move { q.create(&name).await }));
    }
    let mut created = Vec::new();
    for creator in creators {
        created.push(creator.await.unwrap());
    }

    // Release turns in a random order as they start; cancel a few.
    let mut pending: Vec<usize> = (0..TASKS).collect();
    let mut cancelled = Vec::new();
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(60);
    while !pending.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "stalled with {pending:?} left"
        );
        let running: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|i| q.started(&names[*i]))
            .collect();
        if running.is_empty() {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
            continue;
        }
        let pick = running[rng.below(running.len() as u64) as usize];
        if rng.below(6) == 0 {
            q.engine
                .cancel_task(&created[pick].id, false)
                .await
                .unwrap();
            cancelled.push(pick);
        } else {
            q.release(&names[pick]);
        }
        pending.retain(|i| *i != pick);
        tokio::time::sleep(StdDuration::from_millis(rng.below(20))).await;
    }
    // Every queued turn eventually started, and the queue is empty.
    for name in &names {
        q.wait_started(name).await;
    }
    q.wait_rows(0).await;
    // Let everything end (releasing is idempotent), and the slots return.
    for name in &names {
        q.release(name);
    }
    q.wait_running(0).await;
    stop.store(true, Ordering::SeqCst);
    let samples = sampler.await.unwrap();
    assert!(samples > 20, "only {samples} samples");
}

// ---- visibility: the API shapes ----------------------------------------------

#[tokio::test]
async fn a_project_pool_is_listed_with_its_own_limit_and_counts() {
    let q = Q::new(Some(1)).await;
    let capped = q.project("capped", Some(2)).await;
    q.script(&[("job-p1", Ending::Report), ("job-s1", Ending::Report)]);
    q.create_in(&capped.id, "job-p1").await;
    q.create("job-s1").await;
    q.wait_started("job-p1").await;
    q.wait_started("job-s1").await;
    let snapshot = q.engine.queue_snapshot().await.unwrap();
    assert_eq!(snapshot.pools.len(), 2);
    assert_eq!(snapshot.pools[0].pool, "server");
    assert_eq!(
        (snapshot.pools[0].running, snapshot.pools[0].limit),
        (1, Some(1))
    );
    assert_eq!(snapshot.pools[1].pool, format!("project:{}", capped.id));
    assert_eq!(snapshot.pools[1].name, "capped");
    assert_eq!(
        (snapshot.pools[1].running, snapshot.pools[1].limit),
        (1, Some(2))
    );
}

// ---- the dispatcher's other branches ---------------------------------------

#[tokio::test]
async fn a_row_whose_task_has_no_workflow_state_is_withdrawn() {
    let q = Q::new(Some(1)).await;
    let task = q.task_row("job-n").await;
    q.insert_row(&task.id, "coding").await;
    q.wait_rows(0).await;
    let withdrawn = q.queue_events(&task.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn[0]["reason"], "the task has no workflow state");
}

#[tokio::test]
async fn a_row_for_a_stage_that_is_not_an_agent_turn_is_withdrawn() {
    let q = Q::new(Some(1)).await;
    let task = q.task_at("finished", "open").await;
    q.insert_row(&task.id, "finished").await;
    q.wait_rows(0).await;
    let withdrawn = q.queue_events(&task.id, EventType::TurnWithdrawn).await;
    assert!(
        withdrawn[0]["reason"]
            .as_str()
            .unwrap()
            .contains("not an agent_turn"),
        "{withdrawn:?}"
    );
}

#[tokio::test]
async fn a_queued_turn_whose_workflow_cannot_load_parks_the_task_and_deletes_its_row() {
    let q = Q::new(Some(1)).await;
    let task = q
        .task_with("coding", "open", "/no/such/workflow.yaml", json!({}))
        .await;
    q.insert_row(&task.id, "coding").await;
    q.wait_status(&task.id, "stuck").await;
    q.wait_rows(0).await;
    let reason = tasks::get(&q.pool, &task.id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap();
    assert!(
        reason.contains("failed to start after waiting in the queue")
            && reason.contains("could not be loaded"),
        "{reason}"
    );
    q.wait_running(0).await;
}

#[tokio::test]
async fn a_queued_turn_that_fails_to_prepare_parks_the_task_and_deletes_its_row() {
    let q = Q::new(Some(1)).await;
    // The task's own config names a CLI nobody has.
    let task = q
        .task_with(
            "coding",
            "open",
            &q.flow.display().to_string(),
            json!({ "roles": { "coder": { "cli": "no-such-cli" } } }),
        )
        .await;
    q.insert_row(&task.id, "coding").await;
    q.wait_status(&task.id, "stuck").await;
    q.wait_rows(0).await;
    let reason = tasks::get(&q.pool, &task.id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap();
    assert!(reason.contains("no-such-cli"), "{reason}");
    assert!(q.sessions(&task.id).await.is_empty());
    q.wait_running(0).await;
}

/// A group of three branches at cap 1: `one` runs, `two` and `three` wait.
/// `edit` changes what the head row (`two`) says before `one` ends; the head
/// is then refused, withdrawn with a reason containing `why`, and `three`
/// takes the slot.
async fn a_head_row_the_group_no_longer_matches(edit: impl AsyncFnOnce(&Q, &Task), why: &str) {
    let q = Q::new(Some(1)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    assert_eq!(q.rows().await.len(), 2);
    edit(&q, &g).await;
    q.release("one");
    q.wait_started("three").await;
    let withdrawn = q.queue_events(&g.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
    assert_eq!(withdrawn[0]["stage"], "two");
    assert!(withdrawn[0]["group"].is_string(), "{withdrawn:?}");
    assert!(
        withdrawn[0]["reason"].as_str().unwrap().contains(why),
        "{withdrawn:?} should contain {why}"
    );
    // No turn was started for the refused branch.
    assert!(!q.started("two"));
}

#[tokio::test]
async fn a_queued_branch_that_has_already_settled_is_withdrawn() {
    a_head_row_the_group_no_longer_matches(
        async |q, g| {
            // The branch's slot is settled while its row is still queued.
            sqlx::query(
                "UPDATE workflow_state SET payload = \
                 json_set(payload, '$.parallel.branches.two.state', 'failed') \
                 WHERE task_id = ?",
            )
            .bind(&g.id)
            .execute(&q.pool)
            .await
            .unwrap();
        },
        "has already settled",
    )
    .await;
}

#[tokio::test]
async fn a_queued_branch_of_an_earlier_entry_is_withdrawn() {
    a_head_row_the_group_no_longer_matches(
        async |q, _| {
            sqlx::query("UPDATE turn_queue SET group_entry = 99 WHERE stage = 'two'")
                .execute(&q.pool)
                .await
                .unwrap();
        },
        "entered again",
    )
    .await;
}

#[tokio::test]
async fn a_queued_branch_of_another_group_is_withdrawn() {
    a_head_row_the_group_no_longer_matches(
        async |q, _| {
            sqlx::query("UPDATE turn_queue SET group_stage = 'elsewhere' WHERE stage = 'two'")
                .execute(&q.pool)
                .await
                .unwrap();
        },
        "moved on",
    )
    .await;
}

#[tokio::test]
async fn a_queued_branch_that_somehow_has_a_live_session_is_withdrawn() {
    a_head_row_the_group_no_longer_matches(
        async |q, g| {
            sessions::create(
                &q.pool,
                sessions::NewSession {
                    task_id: &g.id,
                    stage: "two",
                    role: "coder",
                    cli_adapter: "claude",
                    model: "sonnet",
                },
            )
            .await
            .unwrap();
        },
        "live session",
    )
    .await;
}

#[tokio::test]
async fn a_queued_branch_the_workflow_no_longer_declares_is_withdrawn() {
    let q = Q::new(Some(1)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    // A running slot and a row for a branch the definition does not have.
    sqlx::query(
        "UPDATE workflow_state SET payload = json_set(payload, '$.parallel.branches.ghost', \
         json('{\"state\":\"running\"}')) WHERE task_id = ?",
    )
    .bind(&g.id)
    .execute(&q.pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM turn_queue")
        .execute(&q.pool)
        .await
        .unwrap();
    turn_queue::insert(
        &q.pool,
        turn_queue::NewQueueRow {
            task_id: &g.id,
            stage: "ghost",
            group_stage: Some("panel"),
            group_entry: Some(1),
            resume_session_id: None,
        },
    )
    .await
    .unwrap();
    q.release("one");
    q.wait_rows(0).await;
    let withdrawn = q.queue_events(&g.id, EventType::TurnWithdrawn).await;
    assert!(
        withdrawn[0]["reason"]
            .as_str()
            .unwrap()
            .contains("no longer in the workflow"),
        "{withdrawn:?}"
    );
}

#[tokio::test]
async fn a_queued_branch_that_fails_to_start_is_failed_in_its_group() {
    let q = Q::new(Some(1)).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Report),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    q.wait_started("one").await;
    // The agent binary goes away: every queued branch fails when admitted.
    fs::remove_file(q.dir.join("fake-claude-queue")).unwrap();
    q.release("one");
    q.wait_status(&g.id, "stuck").await;
    q.wait_rows(0).await;
    let state = workflow_state::get(&q.pool, &g.id).await.unwrap().unwrap();
    for branch in ["two", "three"] {
        assert_eq!(
            state.payload["parallel"]["branches"][branch]["state"], "failed",
            "{branch}"
        );
    }
    q.wait_running(0).await;
}

#[tokio::test]
async fn a_queued_retry_whose_session_cannot_be_resumed_parks_the_task() {
    let q = Q::new(Some(1)).await;
    // A session with no adapter session id: nothing to resume.
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
    turn_queue::insert(
        &q.pool,
        turn_queue::NewQueueRow {
            task_id: &task.id,
            stage: "coding",
            group_stage: None,
            group_entry: None,
            resume_session_id: Some(&old.id),
        },
    )
    .await
    .unwrap();
    q.wait_status(&task.id, "stuck").await;
    let reason = tasks::get(&q.pool, &task.id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap();
    assert!(reason.contains("has no adapter session id"), "{reason}");
    q.wait_rows(0).await;

    // With an adapter session id but no end reason, it is refused too.
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
    sessions::set_adapter_session_id(&q.pool, &old.id, "adapter-1")
        .await
        .unwrap();
    turn_queue::insert(
        &q.pool,
        turn_queue::NewQueueRow {
            task_id: &task.id,
            stage: "coding",
            group_stage: None,
            group_entry: None,
            resume_session_id: Some(&old.id),
        },
    )
    .await
    .unwrap();
    q.wait_status(&task.id, "stuck").await;
    let reason = tasks::get(&q.pool, &task.id)
        .await
        .unwrap()
        .unwrap()
        .stuck_reason
        .unwrap();
    assert!(reason.contains("has no end reason"), "{reason}");
}

#[tokio::test]
async fn a_claim_for_a_task_that_does_not_exist_is_an_error() {
    let q = Q::new(Some(1)).await;
    let spec = TurnSpec {
        stage: "coding",
        group: None,
        resume_session_id: None,
    };
    let err = q
        .engine
        .turn_gate()
        .claim("no-such-task", &spec)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::NoSuchTask), "{err}");
    assert_eq!(q.engine.running_total(), 0);
}

// ---- a row that vanishes mid-admission, and a queue that cannot be written --

#[tokio::test]
async fn a_row_that_vanishes_between_the_pick_and_the_launch_starts_nothing() {
    let q = Q::build(Some(1), queue_timers(), false).await;
    q.script(&[("job-r", Ending::Report)]);
    let task = q.task_at("coding", "open").await;
    let id = q.insert_row(&task.id, "coding").await;
    let picked = q
        .engine
        .turn_gate()
        .pick(Some(id), false)
        .await
        .ok()
        .unwrap();
    let (row, permit) = (picked.row.unwrap(), picked.permit.unwrap());
    assert_eq!(q.engine.running_total(), 1);
    // Cancelled (or withdrawn) in the moment before the session insert.
    assert!(turn_queue::delete(&q.pool, id).await.unwrap());
    let pass = q.engine.admit(row, permit).await;
    assert_eq!(pass, Pass::Progress);
    assert!(q.sessions(&task.id).await.is_empty());
    assert!(!q.started("job-r"), "no process was started");
    assert_eq!(q.engine.running_total(), 0, "the permit was dropped");
    assert_eq!(q.status(&task.id).await, "open", "nothing was failed");
}

#[tokio::test]
async fn a_queue_that_cannot_be_written_leaves_the_dispatcher_idle_and_the_task_stuck() {
    let q = Q::build(None, queue_timers(), false).await;
    // A task whose queued turn will fail to prepare, with a permit in hand.
    let task = q
        .task_with(
            "coding",
            "open",
            &q.flow.display().to_string(),
            json!({ "roles": { "coder": { "cli": "no-such-cli" } } }),
        )
        .await;
    q.insert_row(&task.id, "coding").await;
    let row = q.rows().await.remove(0);
    let spec = TurnSpec {
        stage: "coding",
        group: None,
        resume_session_id: None,
    };
    let Claim::Start(permit) = q.engine.turn_gate().claim(&task.id, &spec).await.unwrap() else {
        panic!("no cap: the turn starts");
    };
    // Now the queue table is gone: deleting the failed turn's row fails.
    sqlx::query("DROP TABLE turn_queue")
        .execute(&q.pool)
        .await
        .unwrap();
    let pass = q.engine.admit(row, permit).await;
    assert_eq!(
        pass,
        Pass::Idle,
        "no hot loop on a row that cannot be deleted"
    );
    assert_eq!(
        q.status(&task.id).await,
        "stuck",
        "the failure is still recorded"
    );
    assert_eq!(q.engine.running_total(), 0);

    // And with the table gone the dispatcher cannot even read the queue: it
    // goes idle rather than spinning.
    let picked = q.engine.turn_gate().pick(None, true).await;
    assert!(
        matches!(picked, Err(crate::engine::turns::PickError::Db(_))),
        "an unreadable queue is a read error"
    );
    assert_eq!(q.engine.dispatch_pass().await, Pass::Idle);
}

#[tokio::test]
async fn withdrawing_a_row_that_cannot_be_deleted_goes_idle() {
    let q = Q::build(None, queue_timers(), false).await;
    let stuck = q.task_at("coding", "stuck").await;
    let spec = TurnSpec {
        stage: "coding",
        group: None,
        resume_session_id: None,
    };
    let Claim::Start(permit) = q.engine.turn_gate().claim(&stuck.id, &spec).await.unwrap() else {
        panic!("no cap: the turn starts");
    };
    q.insert_row(&stuck.id, "coding").await;
    let row = q.rows().await.remove(0);
    sqlx::query("DROP TABLE turn_queue")
        .execute(&q.pool)
        .await
        .unwrap();
    assert_eq!(q.engine.admit(row, permit).await, Pass::Idle);
    assert_eq!(q.engine.running_total(), 0);
}

// ---- retry of a group clears its leftover rows ---------------------------

#[tokio::test]
async fn retrying_a_group_withdraws_a_leftover_row_instead_of_colliding_with_it() {
    let q = Q::build(None, queue_timers(), false).await;
    q.script_branches(&[
        ("BRANCH-ONE", "one", Ending::Report),
        ("BRANCH-TWO", "two", Ending::Crash),
        ("BRANCH-THREE", "three", Ending::Report),
    ]);
    let g = q.create_group().await;
    for name in ["one", "two", "three"] {
        q.wait_started(name).await;
        q.release(name);
    }
    q.wait_status(&g.id, "stuck").await;
    q.wait_running(0).await;
    turn_queue::insert(
        &q.pool,
        turn_queue::NewQueueRow {
            task_id: &g.id,
            stage: "two",
            group_stage: Some("panel"),
            group_entry: Some(1),
            resume_session_id: None,
        },
    )
    .await
    .unwrap();
    fs::remove_file(q.dir.join("release-two")).unwrap();
    fs::remove_file(q.dir.join("started-two")).unwrap();
    q.script_branches(&[("BRANCH-TWO", "two", Ending::Report)]);
    let outcome = q.engine.retry_task(&g.id, RetryMode::Fresh).await.unwrap();
    assert_eq!(outcome.branches[0].queued, None);
    let withdrawn = q.queue_events(&g.id, EventType::TurnWithdrawn).await;
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0]["reason"], "retry");
    assert!(q.rows().await.is_empty());
    q.wait_started("two").await;
}

// ---- a turn's own slot does not hold up its next turn ----------------------

const TWO_STEP_FLOW: &str = "name: qtwo\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: { done: reviewing }\n  reviewing:\n    kind: agent_turn\n    role: coder\n    prompt_file: second.md\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

#[tokio::test]
async fn a_tasks_next_turn_does_not_wait_for_its_own_previous_slot() {
    // Cap 1 and nobody else: when the coder's turn ends, the same task's
    // review turn takes the slot the coder just freed, in the call that
    // advances the task. If the permit were still held while the turn's
    // result is handled, the review would queue behind its own task.
    let q = Q::new(Some(1)).await;
    q.script(&[("job-t", Ending::Report)]);
    let two = q.dir.join("qtwo.yaml");
    fs::write(&two, TWO_STEP_FLOW).unwrap();
    fs::write(q.dir.join("second.md"), "job-t again").unwrap();
    let task = q
        .engine
        .create_task_from(
            &q.project.id,
            WorkflowRef::File(two),
            "two steps",
            "job-t",
            json!({}),
            None,
            &[],
        )
        .await
        .unwrap();
    q.wait_started("job-t").await;
    q.release("job-t");
    q.wait_status(&task.id, "closed").await;
    assert_eq!(q.sessions(&task.id).await.len(), 2, "both turns ran");
    for kind in [
        EventType::TurnQueued,
        EventType::TurnDequeued,
        EventType::TurnWithdrawn,
    ] {
        assert!(
            q.queue_events(&task.id, kind).await.is_empty(),
            "the follow-up turn queued behind its own task: {kind}"
        );
    }
    assert!(q.rows().await.is_empty());
}

// ---- count, decide, take is one critical section -----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_issue_exactly_the_free_slots_and_queue_the_rest_in_order() {
    const CLAIMS: usize = 20;
    // No dispatcher: nothing but `claim` decides who starts.
    let q = Q::build(Some(3), queue_timers(), false).await;
    let mut ids = Vec::new();
    for i in 0..CLAIMS {
        ids.push(q.task_row(&format!("claim-{i}")).await.id);
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(CLAIMS));
    let mut claims = Vec::new();
    for id in ids {
        let (gate, barrier) = (Arc::clone(q.engine.turn_gate()), Arc::clone(&barrier));
        claims.push(tokio::spawn(async move {
            let spec = TurnSpec {
                stage: "coding",
                group: None,
                resume_session_id: None,
            };
            barrier.wait().await;
            gate.claim(&id, &spec).await.unwrap()
        }));
    }
    let mut permits = Vec::new();
    let mut places = Vec::new();
    for claim in claims {
        match claim.await.unwrap() {
            Claim::Start(permit) => permits.push(permit),
            Claim::Queued(at) => places.push(at.position),
        }
    }
    assert_eq!(permits.len(), 3, "exactly the free slots start");
    assert_eq!(q.engine.running_total(), 3);
    places.sort_unstable();
    assert_eq!(
        places,
        (1..=(CLAIMS as u32 - 3)).collect::<Vec<_>>(),
        "the rest queue in distinct places with no gap"
    );
    assert_eq!(q.rows().await.len(), CLAIMS - 3);
}
