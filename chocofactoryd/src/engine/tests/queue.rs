//! Capping agent turns (`max_agents`): the gate, the queue and the dispatcher.
//!
//! The fake agent holds a turn (and so its slot) until the test creates that
//! turn's release file, which makes "who holds a slot now" a thing a test
//! decides.

use super::*;
use crate::db::turn_queue::{self, QueueRow};
use crate::engine::turns::{Claim, RowFate, TurnSpec, is_admissible, queue_row_fate};
use chocofactory_core::models::Project;

const FLOW: &str = "name: qflow\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

const GROUP_FLOW: &str = "name: qgroup\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\nstages:\n  panel:\n    kind: parallel\n    branches:\n      one: { kind: agent_turn, role: coder, prompt_file: one.md }\n      two: { kind: agent_turn, role: coder, prompt_file: two.md }\n      three: { kind: agent_turn, role: coder, prompt_file: three.md }\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

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
        tasks::update_status(&q.pool, &task.id, status).await.unwrap();
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
        notes.into_iter().next().ok_or_else(|| "none yet".to_string())
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
    let q = Q::new(Some(1)).await;
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
    // The server pool has its slot free: its turn starts at once.
    q.create("job-s1").await;
    q.wait_started("job-s1").await;
    q.assert_not_started("job-p2").await;
}

#[tokio::test]
async fn a_project_caps_running_turns_do_not_count_toward_the_server_cap() {
    let q = Q::new(Some(1)).await;
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
    q.create("job-s1").await;
    q.wait_started("job-s1").await;
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
        task.stuck_reason.unwrap().contains("after waiting in the queue"),
        "the reason says the turn had queued"
    );
    q.wait_running(0).await;
    assert!(q.rows().await.is_empty());
    assert!(q.sessions(&b.id).await.iter().all(|s| s.end_reason
        == Some(SessionEndReason::StartFailed)));
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
        let task = tasks::create(
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
        .unwrap();
        task
    }
}
