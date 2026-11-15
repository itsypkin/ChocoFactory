//! Manual retry from a human gate that watches for something of its own
//! (#379): the task goes back to the watcher that timed out, and the gate's
//! watcher is stopped first. The gate here is called `parking_lot`: nothing
//! in the engine may depend on a stage or script name.
//!
//! Each flow reaches the gate through a real timeout: the source watcher's
//! command is held, the engine's clock is moved past the source's timeout
//! and the command released, so the source's last look ends in `timeout`.

use super::*;

#[derive(Clone, Copy, Debug)]
enum Source {
    /// A `poll` stage, as the CI check is.
    Poll,
    /// A `human_gate` with a watcher and a marker, as the review gate is.
    ReviewGate,
}

fn flow_yaml(dir: &Path, source: Source, worktree: bool) -> String {
    let d = dir.display();
    let src_command = format!(
        "echo $$ >> {d}/src_pids; echo x >> {d}/src_count; while [ ! -f {d}/src_go ]; do sleep 0.05; done; echo PENDING"
    );
    let src = match source {
        Source::Poll => format!(
            r#"  src:
    kind: poll
    command: '{src_command}'
    interval: 1h
    timeout: 30m
    outcomes:
      - match: 'NEVER_XYZ'
        then: green
    on: {{ green: done, timeout: parking_lot }}
"#
        ),
        Source::ReviewGate => format!(
            r#"  src:
    kind: human_gate
    capture: text
    markers:
      - line: /approve
        then: approved
    watch:
      command: '{src_command}'
      interval: 1h
      timeout: 30m
      outcomes:
        - match: 'NEVER_XYZ'
          then: approved
    on: {{ approved: done, timeout: parking_lot }}
"#
        ),
    };
    format!(
        r#"name: gr
{worktree}stages:
{src}  parking_lot:
    kind: human_gate
    capture: text
    watch:
      command: 'echo $$ >> {d}/gate_pids; while [ ! -f {d}/gate_release ]; do sleep 0.05; done; echo GATE_OK'
      interval: 1s
      outcomes:
        - match: 'GATE_OK'
          then: ok
    on: {{ resumed: revising, ok: done }}
  other_src:
    kind: poll
    command: 'echo PENDING'
    interval: 1s
    timeout: 30m
    outcomes:
      - match: 'NEVER_XYZ'
        then: green
    on: {{ green: done, timeout: done }}
  revising:
    kind: terminal
  done:
    kind: terminal
"#,
        worktree = if worktree { "worktree: true\n" } else { "" },
    )
}

/// A task parked at `parking_lot` by the source's real timeout, with the
/// gate's command held.
struct Parked {
    pool: SqlitePool,
    dir: TempDir,
    engine: Arc<WorkflowEngine>,
    offset: Arc<std::sync::atomic::AtomicI64>,
    task_id: String,
    gate_pid: u32,
}

fn pids(path: &Path) -> Vec<u32> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect()
}

/// What `ps` says about `pid`: its process group while it exists and is not
/// a zombie waiting to be reaped.
fn live_process_group(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pgid=,stat="])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    let pgid: u32 = fields.next()?.parse().ok()?;
    let stat = fields.next()?;
    (!stat.starts_with('Z')).then_some(pgid)
}

/// Every live member of process group `pgid`, from `ps` alone.
fn group_members(pgid: u32) -> Vec<u32> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,pgid=,stat="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let pid: u32 = f.next()?.parse().ok()?;
            let group: u32 = f.next()?.parse().ok()?;
            let stat = f.next()?;
            (group == pgid && !stat.starts_with('Z')).then_some(pid)
        })
        .collect()
}

async fn wait_process_gone(pid: u32, what: &str) {
    crate::test_support::wait_until(&format!("{what} (pid {pid}) to be gone"), || async {
        match live_process_group(pid) {
            None => Ok(()),
            Some(group) => Err(format!("alive in process group {group}")),
        }
    })
    .await
}

async fn wait_runner_slots(engine: &WorkflowEngine, task_id: &str, slots: usize) {
    crate::test_support::wait_until(&format!("{slots} runner slot(s)"), || async {
        let n = runner_slots(engine, task_id);
        if n == slots {
            Ok(())
        } else {
            Err(format!("{n} slots"))
        }
    })
    .await
}

impl Parked {
    async fn reach(source: Source, worktree: bool) -> Parked {
        let pool = connect_in_memory().await.unwrap();
        let dir = tempdir();
        let yaml = flow_yaml(&dir, source, worktree);
        fs::write(dir.join("gr.yaml"), &yaml).unwrap();
        let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
        let task_id = seed_task_in(&pool, "gr", &dir).await;
        let (engine, offset) = engine_with_offset_clock(pool.clone(), &dir);
        engine.start_task(&task_id, &def, None).await.unwrap();
        wait_until_count(&dir.join("src_count"), 1).await;
        // The source's whole 30m is over by the time its attempt ends.
        offset.store(40 * 60, std::sync::atomic::Ordering::SeqCst);
        fs::write(dir.join("src_go"), "").unwrap();
        wait_until_stage(&pool, &task_id, "parking_lot").await;
        wait_until_count(&dir.join("gate_pids"), 1).await;
        // The source's runner has finished; only the gate's is left.
        wait_runner_slots(&engine, &task_id, 1).await;
        let gate_pid = pids(&dir.join("gate_pids"))[0];
        Parked {
            pool,
            dir,
            engine,
            offset,
            task_id,
            gate_pid,
        }
    }

    /// The timed-out source's wiring is what makes this gate retryable.
    async fn arrival(&self) -> Value {
        state_of(&self.pool, &self.task_id).await.payload["arrival"].clone()
    }

    /// Replaces `payload.arrival` and nothing else.
    async fn set_arrival(&self, arrival: Option<Value>) {
        let state = state_of(&self.pool, &self.task_id).await;
        let mut payload = state.payload.clone();
        match arrival {
            Some(arrival) => payload["arrival"] = arrival,
            None => {
                payload.as_object_mut().unwrap().remove("arrival");
            }
        }
        workflow_state::update(
            &self.pool,
            &self.task_id,
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
    }
}

#[tokio::test]
async fn retry_from_a_watched_timeout_gate_replaces_the_gate_watcher() {
    for source in [Source::Poll, Source::ReviewGate] {
        let p = Parked::reach(source, false).await;
        assert_eq!(
            p.arrival().await,
            json!({ "from": "src", "outcome": "timeout" }),
            "{source:?}"
        );
        // Counters and captures that retry must carry over, not two empty ones.
        let mut state = state_of(&p.pool, &p.task_id).await;
        let mut payload = state.payload.clone();
        payload["stages"] = json!({ "earlier": "a capture that stays" });
        let counters = json!({ "src": { "count": 2 }, "other": { "count": 1 } });
        workflow_state::update(
            &p.pool,
            &p.task_id,
            workflow_state::WorkflowStateUpdate {
                current_stage: "parking_lot".into(),
                stage_kind: "human_gate".into(),
                loop_counters: counters.clone(),
                payload,
                enters_stage: false,
            },
        )
        .await
        .unwrap();
        state = state_of(&p.pool, &p.task_id).await;
        assert!(state.payload["left_at"]["src"].is_string(), "{source:?}");
        let old_window = poll_window_for(&state.payload, "parking_lot")
            .unwrap()
            .unwrap();

        // The gate's command is really held, as the OS sees it: its pid is
        // alive and leads a process group that still holds its `sleep`.
        assert_eq!(
            live_process_group(p.gate_pid),
            Some(p.gate_pid),
            "{source:?}"
        );
        assert!(!group_members(p.gate_pid).is_empty(), "{source:?}");

        // Hold the replacement source attempt, then retry.
        fs::remove_file(p.dir.join("src_go")).unwrap();
        let outcome = p
            .engine
            .retry_task(&p.task_id, RetryMode::Auto)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            RetryOutcome {
                stage: "src".into(),
                resumed: false,
                adapter_session_id: None,
                fresh_reason: None,
                rewatched: true,
                branches: Vec::new(),
                queued: None,
            },
            "{source:?}"
        );
        // The source's replacement attempt is the third run of its command.
        wait_until_count(&p.dir.join("src_count"), 3).await;

        // The old gate command and its whole process group are gone.
        wait_process_gone(p.gate_pid, "the old gate command").await;
        assert!(group_members(p.gate_pid).is_empty(), "{source:?}");
        // Exactly one live runner, and it is the replacement's.
        assert_eq!(runner_slots(&p.engine, &p.task_id), 1, "{source:?}");
        let src_pids = pids(&p.dir.join("src_pids"));
        let replacement = *src_pids.last().unwrap();
        assert_eq!(src_pids.len(), 3, "{source:?}");
        assert_ne!(replacement, p.gate_pid, "{source:?}");
        assert!(live_process_group(replacement).is_some(), "{source:?}");
        assert_eq!(pids(&p.dir.join("gate_pids")), [p.gate_pid], "{source:?}");

        let after = state_of(&p.pool, &p.task_id).await;
        assert_eq!(after.current_stage, "src", "{source:?}");
        assert_eq!(
            after.stage_kind.as_deref(),
            Some(if matches!(source, Source::Poll) {
                "poll"
            } else {
                "human_gate"
            })
        );
        assert_eq!(
            after.payload["arrival"],
            json!({ "from": "parking_lot", "outcome": "retry" }),
            "{source:?}"
        );
        assert_eq!(after.loop_counters, counters, "{source:?}");
        assert_eq!(
            after.payload["stages"], state.payload["stages"],
            "{source:?}"
        );
        assert_eq!(
            after.payload["left_at"], state.payload["left_at"],
            "{source:?}"
        );
        let window = poll_window_for(&after.payload, "src").unwrap().unwrap();
        assert!(window.entered_at >= old_window.entered_at, "{source:?}");
        assert_eq!(
            window.deadline,
            Some(window.entered_at + chrono::Duration::minutes(30)),
            "{source:?}"
        );
        assert!(
            window.entered_at > Utc::now() + chrono::Duration::minutes(39),
            "a fresh window starts at the engine's clock, 40m ahead: {window:?}"
        );
        let task = tasks::get(&p.pool, &p.task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "open", "{source:?}");
        assert!(
            sessions::list_for_task(&p.pool, &p.task_id)
                .await
                .unwrap()
                .is_empty(),
            "no coder lap: {source:?}"
        );

        // A stale gate attempt cannot decide the replacement: let what was
        // the gate's command finish, and the task stays at the source.
        fs::write(p.dir.join("gate_release"), "").unwrap();
        tokio::time::sleep(StdDuration::from_secs(2)).await;
        assert_eq!(
            state_of(&p.pool, &p.task_id).await.current_stage,
            "src",
            "{source:?}"
        );
        assert_eq!(runner_slots(&p.engine, &p.task_id), 1, "{source:?}");
        let _ = &p.offset;
        p.engine.abort_detached_runners(&p.task_id).await;
    }
}

#[tokio::test]
async fn a_refused_retry_leaves_a_watched_gate_and_its_watcher_alone() {
    let p = Parked::reach(Source::ReviewGate, false).await;
    let timeout = json!({ "from": "src", "outcome": "timeout" });
    let cases: Vec<(&str, Option<Value>)> = vec![
        ("no arrival", None),
        ("a null arrival", Some(Value::Null)),
        ("a string arrival", Some(json!("timeout"))),
        ("an array arrival", Some(json!(["src", "timeout"]))),
        ("no outcome", Some(json!({ "from": "src" }))),
        (
            "a null outcome",
            Some(json!({ "from": "src", "outcome": null })),
        ),
        (
            "a numeric outcome",
            Some(json!({ "from": "src", "outcome": 7 })),
        ),
        ("no source", Some(json!({ "outcome": "timeout" }))),
        (
            "a null source",
            Some(json!({ "from": null, "outcome": "timeout" })),
        ),
        (
            "a numeric source",
            Some(json!({ "from": 7, "outcome": "timeout" })),
        ),
        (
            "a loop guard",
            Some(json!({ "from": "src", "outcome": "changes_requested" })),
        ),
        (
            "an error",
            Some(json!({ "from": "src", "outcome": "error" })),
        ),
        (
            "a different spelling of timeout",
            Some(json!({ "from": "src", "outcome": "Timeout" })),
        ),
        (
            "a source that does not exist",
            Some(json!({ "from": "ghost", "outcome": "timeout" })),
        ),
        (
            "a source that does not watch",
            Some(json!({ "from": "done", "outcome": "timeout" })),
        ),
        (
            "a source whose timeout edge leads elsewhere",
            Some(json!({ "from": "other_src", "outcome": "timeout" })),
        ),
    ];
    for (label, arrival) in cases {
        p.set_arrival(arrival).await;
        let before = state_of(&p.pool, &p.task_id).await;
        let err = p
            .engine
            .retry_task(&p.task_id, RetryMode::Auto)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RetryTaskError::NotStuck(status) if status == "open"),
            "{label}: {err:?}"
        );
        assert_eq!(state_of(&p.pool, &p.task_id).await, before, "{label}");
        assert_eq!(runner_slots(&p.engine, &p.task_id), 1, "{label}");
        assert!(live_process_group(p.gate_pid).is_some(), "{label}");
    }

    // A real timeout arrival, but a mode, a pause or a closed task.
    p.set_arrival(Some(timeout)).await;
    for mode in [RetryMode::Resume, RetryMode::Fresh] {
        let before = state_of(&p.pool, &p.task_id).await;
        let err = p.engine.retry_task(&p.task_id, mode).await.unwrap_err();
        assert!(
            matches!(&err, RetryTaskError::RewatchTakesNoMode { stage, .. } if stage == "src"),
            "{mode:?}: {err:?}"
        );
        assert_eq!(state_of(&p.pool, &p.task_id).await, before);
        assert_eq!(runner_slots(&p.engine, &p.task_id), 1);
        assert!(live_process_group(p.gate_pid).is_some());
    }
    for status in ["pausing", "paused"] {
        tasks::update_status(&p.pool, &p.task_id, status)
            .await
            .unwrap();
        let before = state_of(&p.pool, &p.task_id).await;
        let err = p
            .engine
            .retry_task(&p.task_id, RetryMode::Auto)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RetryTaskError::Paused { status: s, .. } if s == status),
            "{status}: {err:?}"
        );
        assert_eq!(state_of(&p.pool, &p.task_id).await, before);
        assert_eq!(runner_slots(&p.engine, &p.task_id), 1);
        assert!(live_process_group(p.gate_pid).is_some());
    }
    for status in ["closed", "cancelled"] {
        tasks::update_status(&p.pool, &p.task_id, status)
            .await
            .unwrap();
        let err = p
            .engine
            .retry_task(&p.task_id, RetryMode::Auto)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RetryTaskError::NotStuck(s) if s == status),
            "{status}: {err:?}"
        );
        assert!(live_process_group(p.gate_pid).is_some());
    }
    p.engine.abort_detached_runners(&p.task_id).await;
}

#[tokio::test]
async fn a_retry_whose_state_update_fails_leaves_the_gate_and_its_watcher_running() {
    let p = Parked::reach(Source::Poll, false).await;
    let before = state_of(&p.pool, &p.task_id).await;
    sqlx::query(
        "CREATE TRIGGER fail_the_rewatch_write BEFORE UPDATE ON workflow_state \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .execute(&p.pool)
    .await
    .unwrap();
    fs::remove_file(p.dir.join("src_go")).unwrap();
    let err = p
        .engine
        .retry_task(&p.task_id, RetryMode::Auto)
        .await
        .unwrap_err();
    assert!(matches!(err, RetryTaskError::Db(_)), "{err:?}");
    // State, window and counters are as they were, and the gate's watcher
    // is the one that was running: same process, one slot, no source run.
    assert_eq!(state_of(&p.pool, &p.task_id).await, before);
    assert_eq!(runner_slots(&p.engine, &p.task_id), 1);
    assert_eq!(live_process_group(p.gate_pid), Some(p.gate_pid));
    assert_eq!(pids(&p.dir.join("gate_pids")), [p.gate_pid]);
    assert_eq!(
        count_lines(&p.dir.join("src_count")),
        2,
        "no new source attempt"
    );
    let task = tasks::get(&p.pool, &p.task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "open");
    p.engine.abort_detached_runners(&p.task_id).await;
}

#[tokio::test]
async fn a_rewatch_that_cannot_start_the_source_leaves_it_stuck_with_the_gate_watcher_gone() {
    // The flow wants a worktree, which the task has: at first. Once the
    // gate is running, the task's recorded worktree is made unusable, so
    // the gate keeps running but the source cannot start.
    let pool = connect_in_memory().await.unwrap();
    let repo = tempdir();
    init_git_repo(&repo).await;
    let dir = tempdir();
    let yaml = flow_yaml(&dir, Source::Poll, true);
    fs::write(dir.join("gr.yaml"), &yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(&yaml, &dir).unwrap());
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let task_id = tasks::create(
        &pool,
        tasks::NewTask {
            project_id: &project_id,
            workflow_def: "gr",
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
    worktree::ensure(&repo, "demo", &task_id, None)
        .await
        .unwrap();
    tasks::set_worktree(&pool, &task_id, &repo.to_string_lossy(), "demo")
        .await
        .unwrap();
    let (engine, offset) = engine_with_offset_clock(pool.clone(), &dir);
    // The commands write to `dir`, not to the worktree they run in.
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_count(&dir.join("src_count"), 1).await;
    offset.store(40 * 60, std::sync::atomic::Ordering::SeqCst);
    fs::write(dir.join("src_go"), "").unwrap();
    wait_until_stage(&pool, &task_id, "parking_lot").await;
    wait_until_count(&dir.join("gate_pids"), 1).await;
    wait_runner_slots(&engine, &task_id, 1).await;
    let gate_pid = pids(&dir.join("gate_pids"))[0];

    sqlx::query("UPDATE tasks SET worktree_repo = 'not/an/absolute/path' WHERE id = ?")
        .bind(&task_id)
        .execute(&pool)
        .await
        .unwrap();
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
            .unwrap_or_default()
            .starts_with("stage 'src': retry failed"),
        "{:?}",
        task.stuck_reason
    );
    assert_eq!(state_of(&pool, &task_id).await.current_stage, "src");
    // The gate's watcher was stopped before the source was entered, and
    // nothing replaced it.
    wait_process_gone(gate_pid, "the old gate command").await;
    assert_eq!(runner_slots(&engine, &task_id), 0);
    let _ = worktree::remove(&repo, "demo", &task_id).await;
}

#[tokio::test]
async fn status_and_marker_refusal_describe_the_original_watcher_at_a_watched_gate() {
    let p = Parked::reach(Source::ReviewGate, false).await;
    let task = tasks::get(&p.pool, &p.task_id).await.unwrap().unwrap();
    let state = state_of(&p.pool, &p.task_id).await;
    assert_eq!(
        p.engine.watch_timed_out(&task, &state).await,
        Some(WatchTimedOutInfo {
            stage: "src".into(),
            timeout_secs: Some(1800),
            resumes_to: Some("revising".into()),
        })
    );
    // A reply that is only the review gate's marker is refused at this
    // gate, naming retry, and leaves everything — and the watcher — as is.
    let before = state_of(&p.pool, &p.task_id).await;
    let err = p
        .engine
        .send_message_or_resume(&p.task_id, "/approve")
        .await
        .unwrap_err();
    match &err {
        SendMessageOrResumeError::ReplyIsOnlyMarkers {
            stage,
            resumes_to,
            rewatch,
            ..
        } => {
            assert_eq!(stage, "parking_lot");
            assert_eq!(resumes_to.as_deref(), Some("revising"));
            assert!(*rewatch);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        err.to_string()
            .contains(&format!("choco task retry {}", p.task_id))
    );
    assert!(human_messages(&p.pool, &p.task_id).await.is_empty());
    assert_eq!(state_of(&p.pool, &p.task_id).await, before);
    assert_eq!(runner_slots(&p.engine, &p.task_id), 1);
    assert!(live_process_group(p.gate_pid).is_some());

    // Arrived some other way: no retry hint, and the refusal does not name it.
    p.set_arrival(Some(json!({ "from": "src", "outcome": "error" })))
        .await;
    let task = tasks::get(&p.pool, &p.task_id).await.unwrap().unwrap();
    let state = state_of(&p.pool, &p.task_id).await;
    assert_eq!(p.engine.watch_timed_out(&task, &state).await, None);
    let err = p
        .engine
        .send_message_or_resume(&p.task_id, "/approve")
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
    p.engine.abort_detached_runners(&p.task_id).await;
}
