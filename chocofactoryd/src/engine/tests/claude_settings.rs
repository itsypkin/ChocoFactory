//! A role's `claude_settings` (#346) through the engine: where a refusal
//! happens and what it leaves behind, and what a settings bundle Claude Code
//! rejects does to the task.
//!
//! The fake claude runs behind a `/bin/sh` wrapper that records its spawn and
//! its pid and reads its mode from a file in the test dir, never from the test
//! process's environment. Its tool server is the real `choco mcp-serve`, so the
//! daemon's check is tested against the real verdict.

use super::*;
use crate::adapter::OmpAdapter;
use crate::db::{connect_in_memory, events, projects, sessions, tasks};
use chocofactory_core::models::{RetryMode, SessionEndReason};

/// A gate in front of one agent turn, so config.yaml can be edited after
/// create and before the turn.
fn gated(role_extra: &str) -> String {
    gated_on("claude", role_extra)
}

fn gated_on(cli: &str, role_extra: &str) -> String {
    format!(
        "name: gated\nworktree: false\nroles:\n  coder:\n    cli: {cli}\n    model: sonnet\n{role_extra}stages:\n  review:\n    kind: human_gate\n    on: {{ resumed: coding }}\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: p.md\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
    )
}

struct Env {
    dir: TempDir,
    pool: SqlitePool,
    project_id: String,
    engine: Arc<WorkflowEngine>,
    global: PathBuf,
    omp_marker: PathBuf,
}

async fn env() -> Env {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    // Production-shaped: under a home's `.config/chocofactory/`.
    let config_dir = dir.join("home/.config/chocofactory");
    fs::create_dir_all(&config_dir).unwrap();
    let global = config_dir.join("config.yaml");
    fs::write(dir.join("mode"), "apply").unwrap();
    let wrapper = dir.join("claude-wrapper");
    crate::test_support::write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\ntouch '{d}/spawned'\necho $$ > '{d}/pid'\nexport FAKE_CLAUDE_SETTINGS=\"$(cat '{d}/mode')\"\nexport FAKE_CLAUDE_PROMPT_MARKER='{d}/prompt'\nexport FAKE_CLAUDE_ARGV_FILE=\"{d}/argv-$(date +%s)-$$.json\"\nif [ -e '{d}/lingers' ]; then\n  '{fixture}' \"$@\"\n  sleep 60\n  exit\nfi\nexec '{fixture}' \"$@\"\n",
            d = dir.display(),
            fixture = fixture_binary("fake_claude.py"),
        ),
    );
    let omp_marker = dir.join("omp-was-run");
    let omp_wrapper = dir.join("omp-wrapper");
    crate::test_support::write_executable(
        &omp_wrapper,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", omp_marker.display()),
    );
    let registry = Registry::new(vec![
        Arc::new(
            ClaudeAdapter::with_binary(wrapper.display().to_string())
                .with_choco_binary(workspace_binary("choco").display().to_string()),
        ),
        Arc::new(OmpAdapter::with_binary(
            omp_wrapper.display().to_string(),
            dir.join("omp-state"),
        )),
    ]);
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), registry, &dir, Some(&global));
    Env {
        dir,
        pool,
        project_id,
        engine,
        global,
        omp_marker,
    }
}

impl Env {
    async fn create(&self, yaml: &str, config: Value) -> Result<Task, CreateTaskError> {
        fs::write(self.dir.join("p.md"), "do it").unwrap();
        let flow = self.dir.join("flow.yaml");
        fs::write(&flow, yaml).unwrap();
        self.engine
            .create_task_from(
                &self.project_id,
                WorkflowRef::File(flow),
                "T",
                "go",
                config,
                None,
                &[],
            )
            .await
    }

    async fn task_count(&self) -> usize {
        tasks::list(&self.pool, Some(&self.project_id), None)
            .await
            .unwrap()
            .len()
    }

    async fn task(&self, id: &str) -> Task {
        tasks::get(&self.pool, id).await.unwrap().unwrap()
    }

    fn spawned(&self) -> bool {
        self.dir.join("spawned").exists()
    }

    fn prompt_written(&self) -> bool {
        self.dir.join("prompt").exists()
    }

    /// The sentinel names the spawns' argv files hold, in file-name order.
    fn sentinel_names(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = fs::read_dir(&*self.dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("argv-"))
            })
            .collect();
        files.sort();
        files
            .iter()
            .filter_map(|file| {
                let argv: Vec<String> =
                    serde_json::from_str(&fs::read_to_string(file).ok()?).ok()?;
                sentinel_in_argv(&argv).map(|(name, _)| name)
            })
            .collect()
    }
}

/// The sentinel `(name, value)` in a spawn's `--mcp-config`.
fn sentinel_in_argv(argv: &[String]) -> Option<(String, String)> {
    let config = argv.get(argv.iter().position(|a| a == "--mcp-config")? + 1)?;
    let config: Value = serde_json::from_str(config).ok()?;
    let args = config["mcpServers"]["chocofactory"]["args"].as_array()?;
    let flag = args.get(
        args.iter()
            .position(|a| a == "--claude-settings-sentinel")?
            + 1,
    )?;
    let (name, value) = flag.as_str()?.split_once('=')?;
    Some((name.to_string(), value.to_string()))
}

async fn all_event_text(pool: &SqlitePool, task_id: &str) -> String {
    let mut text = String::new();
    for session in sessions::list_for_task(pool, task_id).await.unwrap() {
        for event in events::list_for_session(pool, &session.id).await.unwrap() {
            text.push_str(&event.payload.to_string());
            text.push('\n');
        }
    }
    text
}

const D4B_STUCK: &str = "role 'coder' sets claude_settings 'hooks' in the global config, which only a workflow file may set";

/// D4b: a workflow-only key edited into config.yaml while the daemon runs
/// parks the role's next turn, with nothing left behind, and a fixed file
/// lets `retry` run the turn.
#[tokio::test]
async fn a_workflow_only_key_edited_into_config_parks_the_turn_with_nothing_left_behind() {
    let env = env().await;
    let task = env.create(&gated(""), json!({})).await.unwrap();
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      attribution: {commit: attr-secret-d4b}\n      hooks:\n        SessionStart:\n          - hooks:\n              - {type: command, command: echo hook-secret-d4b}\n",
    )
    .unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;

    let after = env.task(&task.id).await;
    assert_eq!(after.status, "stuck");
    let reason = after.stuck_reason.unwrap();
    assert!(reason.contains(D4B_STUCK), "{reason}");
    for leaked in ["attribution", "attr-secret-d4b", "hook-secret-d4b"] {
        assert!(!reason.contains(leaked), "{leaked} leaked: {reason}");
    }
    assert!(
        sessions::list_for_task(&env.pool, &task.id)
            .await
            .unwrap()
            .is_empty(),
        "no session row"
    );
    assert!(!env.spawned(), "nothing was spawned");
    assert_eq!(after.worktree_repo, None);
    assert_eq!(after.worktree_project, None);

    // Fix the file (keep `attribution`), retry: the turn starts, passes the
    // settings check and completes.
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      attribution: {commit: attr-secret-d4b}\n",
    )
    .unwrap();
    env.engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap();
    wait_until_task_status(&env.pool, &task.id, "closed").await;
    assert!(env.spawned());
    assert!(env.prompt_written(), "the gate opened and the prompt went");
    assert_eq!(
        sessions::list_for_task(&env.pool, &task.id)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// R8: the refusal is per role. A turn of another role on the same daemon
/// still starts while `coder` holds a workflow-only key.
#[tokio::test]
async fn a_workflow_only_key_in_one_roles_config_does_not_stop_another_roles_turn() {
    let env = env().await;
    let yaml = "name: two\nworktree: false\nroles:\n  coder:\n    cli: claude\n    model: sonnet\n  helper:\n    cli: claude\n    model: sonnet\nstages:\n  review:\n    kind: human_gate\n    on: { resumed: helping }\n  helping:\n    kind: agent_turn\n    role: helper\n    prompt_file: p.md\n    on: { done: coding }\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: p.md\n    on: { done: finished }\n  finished:\n    kind: terminal\n";
    let task = env.create(yaml, json!({})).await.unwrap();
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      hooks: {}\n",
    )
    .unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    wait_until_task_status(&env.pool, &task.id, "stuck").await;
    let after = env.task(&task.id).await;
    let reason = after.stuck_reason.unwrap();
    assert!(reason.contains(D4B_STUCK), "{reason}");
    let sessions = sessions::list_for_task(&env.pool, &task.id).await.unwrap();
    assert_eq!(sessions.len(), 1, "only the helper's turn ran");
    assert_eq!(sessions[0].role, "helper");
}

#[tokio::test]
async fn create_and_reconfigure_refuse_a_workflow_only_key_in_the_global_config() {
    let env = env().await;
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      outputStyle: x\n      attribution: {commit: ''}\n",
    )
    .unwrap();
    let err = env.create(&gated(""), json!({})).await.unwrap_err();
    assert!(matches!(err, CreateTaskError::RoleRejected(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "role 'coder' sets claude_settings 'outputStyle' in the global config, which only a workflow file may set"
    );
    assert_eq!(env.task_count().await, 0);

    // A task created while the file is clean; the edit lands afterwards and a
    // patch touching that role is refused.
    fs::write(&env.global, "").unwrap();
    let task = env.create(&gated(""), json!({})).await.unwrap();
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      hooks: {}\n",
    )
    .unwrap();
    let err = env
        .engine
        .check_config_patch(&task.id, &json!({"roles": {"coder": {"model": "sonnet"}}}))
        .await
        .unwrap_err();
    let ConfigPatchError::Rejected(message) = err else {
        panic!("{err:?}");
    };
    assert_eq!(message, D4B_STUCK);
}

/// R5 at create and at reconfigure: the field comes only from config.yaml,
/// the omp cli only from the task's config.
#[tokio::test]
async fn omp_with_claude_settings_is_refused_at_create_and_at_reconfigure() {
    let env = env().await;
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      attribution: {commit: omp-secret}\n",
    )
    .unwrap();
    let err = env
        .create(&gated(""), json!({"roles": {"coder": {"cli": "omp"}}}))
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::RoleRejected(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "role 'coder' sets claude_settings, which a role on cli omp can't take; claude_settings is only for claude roles"
    );
    assert!(!err.to_string().contains("omp-secret"));
    assert_eq!(env.task_count().await, 0);

    let task = env.create(&gated(""), json!({})).await.unwrap();
    let err = env
        .engine
        .check_config_patch(&task.id, &json!({"roles": {"coder": {"cli": "omp"}}}))
        .await
        .unwrap_err();
    let ConfigPatchError::Rejected(message) = err else {
        panic!("{err:?}");
    };
    assert!(
        message.starts_with("role 'coder' sets claude_settings"),
        "{message}"
    );
}

/// R5 at turn start: a config.yaml edited after create adds the field to a
/// role that runs on omp. The omp binary is never run.
#[tokio::test]
async fn omp_with_claude_settings_edited_in_after_create_parks_the_turn() {
    let env = env().await;
    let yaml = gated_on("omp", "");
    let task = env.create(&yaml, json!({})).await.unwrap();
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      attribution: {commit: ''}\n",
    )
    .unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    let after = env.task(&task.id).await;
    assert_eq!(after.status, "stuck");
    let reason = after.stuck_reason.unwrap();
    assert!(
        reason.contains("role 'coder' sets claude_settings, which a role on cli omp can't take"),
        "{reason}"
    );
    assert!(
        sessions::list_for_task(&env.pool, &task.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!env.omp_marker.exists(), "omp was never started");
}

/// A role `env` that is not a mapping, edited into config.yaml after create:
/// the turn parks at start, with nothing spawned and the value not echoed.
#[tokio::test]
async fn a_non_mapping_env_edited_in_after_create_parks_the_turn_at_start() {
    let env = env().await;
    let task = env.create(&gated(""), json!({})).await.unwrap();
    fs::write(
        &env.global,
        "roles:\n  coder:\n    claude_settings:\n      env: env-secret-x\n",
    )
    .unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    let after = env.task(&task.id).await;
    assert_eq!(after.status, "stuck");
    let reason = after.stuck_reason.unwrap();
    assert!(
        reason.contains("role 'coder': claude_settings 'env' must be a mapping"),
        "{reason}"
    );
    assert!(!reason.contains("env-secret-x"), "{reason}");
    assert!(
        sessions::list_for_task(&env.pool, &task.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!env.spawned());
}

const R9_STUCK: &str = "stage 'coding': Claude Code did not apply role 'coder''s claude_settings (it ignores the whole settings when one value is invalid), so choco stopped the turn; fix the value and run 'choco task retry'";

/// R9: a bundle Claude Code rejects parks the task `stuck`, the agent never
/// gets its prompt, and a retry (fresh, with a new sentinel) completes once
/// the cause is gone.
#[tokio::test]
async fn rejected_settings_park_the_task_and_the_prompt_is_never_written() {
    let env = env().await;
    fs::write(env.dir.join("mode"), "reject").unwrap();
    let yaml = gated(
        "    claude_settings:\n      attribution: {commit: r9-c}\n      env: {VISIBLE: v-1}\n",
    );
    let task = env.create(&yaml, json!({})).await.unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    wait_until_task_status(&env.pool, &task.id, "stuck").await;

    let after = env.task(&task.id).await;
    assert_eq!(after.stuck_reason.as_deref(), Some(R9_STUCK));
    let rows = sessions::list_for_task(&env.pool, &task.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].end_reason,
        Some(SessionEndReason::ClaudeSettingsRejected)
    );
    assert!(!env.prompt_written(), "the prompt must never be written");
    let timeline = events::list_for_session(&env.pool, &rows[0].id)
        .await
        .unwrap();
    assert!(
        !timeline
            .iter()
            .any(|e| e.payload.to_string().contains("report_outcome")),
        "the turn must not report"
    );
    assert!(
        timeline.iter().any(|e| e.payload["message"]
            == "Claude Code did not apply role 'coder''s claude_settings: it ignores the whole settings when one value is invalid. choco stopped the turn."),
        "{timeline:?}"
    );

    // The sentinel is recorded nowhere: not in events, not in session_meta,
    // not in the stuck reason.
    let names = env.sentinel_names();
    assert_eq!(names.len(), 1);
    let argv_file = fs::read_dir(&*env.dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("argv-")
        })
        .unwrap();
    let argv: Vec<String> = serde_json::from_str(&fs::read_to_string(argv_file).unwrap()).unwrap();
    let (name, value) = sentinel_in_argv(&argv).unwrap();
    let text = all_event_text(&env.pool, &task.id).await;
    for leaked in ["CHOCOFACTORY_SETTINGS_", name.as_str(), value.as_str()] {
        assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        assert!(!R9_STUCK.contains(leaked));
    }
    let meta = timeline
        .iter()
        .find(|e| e.event_type == chocofactory_core::models::EventType::SessionMeta)
        .expect("a session_meta");
    assert_eq!(
        meta.payload["claude_settings_keys"],
        json!(["attribution", "env"])
    );

    // Retry once the cause is gone: a fresh session, a new sentinel, done.
    fs::write(env.dir.join("mode"), "apply").unwrap();
    env.engine
        .retry_task(&task.id, RetryMode::Fresh)
        .await
        .unwrap();
    wait_until_task_status(&env.pool, &task.id, "closed").await;
    let rows = sessions::list_for_task(&env.pool, &task.id).await.unwrap();
    assert_eq!(rows.len(), 2);
    let fresh = rows
        .iter()
        .find(|s| s.end_reason != Some(SessionEndReason::ClaudeSettingsRejected))
        .unwrap();
    assert_eq!(fresh.resumed_from, None);
    assert!(env.prompt_written());
    let names = env.sentinel_names();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1], "a new sentinel per spawn");
}

/// Decision 22: on a miss choco kills the whole process group at once, rather
/// than relying on the CLI exiting when its stdin closes. The wrapper keeps
/// the stdout pipe open for a minute after the fake exits (as a lingering hook
/// or child would), so only the kill ends the session in time.
#[tokio::test]
async fn a_rejected_turn_kills_a_process_that_lingers_after_stdin_closes() {
    let env = env().await;
    fs::write(env.dir.join("mode"), "reject").unwrap();
    fs::write(env.dir.join("lingers"), "").unwrap();
    let yaml = gated("    claude_settings:\n      attribution: {commit: c}\n");
    let task = env.create(&yaml, json!({})).await.unwrap();
    let started = std::time::Instant::now();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    wait_until_task_status(&env.pool, &task.id, "stuck").await;
    assert!(
        started.elapsed() < StdDuration::from_secs(30),
        "the session waited for the lingering process: {:?}",
        started.elapsed()
    );
    let rows = sessions::list_for_task(&env.pool, &task.id).await.unwrap();
    assert_eq!(
        rows[0].end_reason,
        Some(SessionEndReason::ClaudeSettingsRejected)
    );
    let pid: i32 = fs::read_to_string(env.dir.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the agent process {pid} is gone");
}

/// A CLI that never answers `mcp_status`: the turn waits at the gate and a
/// cancel ends it, leaving no process and no prompt behind.
#[tokio::test]
async fn cancelling_a_turn_waiting_at_the_gate_leaves_nothing_behind() {
    let env = env().await;
    fs::write(env.dir.join("mode"), "silent").unwrap();
    let yaml = gated("    claude_settings:\n      attribution: {commit: c}\n");
    let task = env.create(&yaml, json!({})).await.unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    crate::test_support::wait_until("the gated session to exist", || async {
        match sessions::list_for_task(&env.pool, &task.id)
            .await
            .unwrap()
            .first()
        {
            Some(s) if env.dir.join("pid").exists() => Ok(s.id.clone()),
            _ => Err("no session yet".to_string()),
        }
    })
    .await;
    let pid: i32 = fs::read_to_string(env.dir.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(500)).await;
    assert!(!env.prompt_written(), "no prompt while the CLI is silent");

    env.engine.cancel_task(&task.id, false).await.unwrap();
    wait_until_task_status(&env.pool, &task.id, "cancelled").await;
    crate::test_support::wait_until("the session to end", || async {
        let rows = sessions::list_for_task(&env.pool, &task.id).await.unwrap();
        match rows.first() {
            Some(s) if s.end_reason == Some(SessionEndReason::Cancelled) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    })
    .await;
    assert!(!env.prompt_written());
    // SAFETY-free liveness probe: signal 0 only checks that the pid exists.
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the agent process {pid} is gone");
}

fn load(env: &Env, yaml: &str) -> Result<(), WorkflowDefError> {
    fs::write(env.dir.join("p.md"), "do it").unwrap();
    let path = env.dir.join("load.yaml");
    fs::write(&path, yaml).unwrap();
    load_workflow_file(&path, env.engine.registry()).map(|_| ())
}

/// R5 at workflow load: the workflow's own field on a role with `cli: omp`.
#[tokio::test]
async fn a_workflow_role_on_omp_with_claude_settings_fails_to_load_naming_the_role() {
    let env = env().await;
    let err = load(
        &env,
        &gated_on(
            "omp",
            "    claude_settings:\n      attribution: {commit: ''}\n",
        ),
    )
    .unwrap_err();
    assert!(matches!(err, WorkflowDefError::RoleRejected(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "role 'coder' sets claude_settings, which a role on cli omp can't take; claude_settings is only for claude roles"
    );
    load(&env, &gated_on("omp", "    claude_settings: {}\n")).unwrap();
}

/// An isolated role's `env` must be a mapping; chat's may be anything.
#[tokio::test]
async fn an_isolated_workflow_role_with_a_non_mapping_env_fails_to_load_and_chat_loads() {
    let env = env().await;
    let err = load(&env, &gated("    claude_settings:\n      env: [a]\n")).unwrap_err();
    assert!(matches!(err, WorkflowDefError::RoleRejected(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "role 'coder': claude_settings 'env' must be a mapping, because choco adds a variable to it to check that Claude Code applied the settings"
    );
    load(
        &env,
        &gated("    inherit_operator_config: true\n    claude_settings:\n      env: x\n"),
    )
    .unwrap();
    load(&env, &gated("    claude_settings:\n      env: {A: b}\n")).unwrap();
}

/// A CLI that refuses the `mcp_status` request: choco cannot confirm, so the
/// turn stops before the prompt and says so without claiming why. The
/// refusal's own text and the sentinel are recorded nowhere.
#[tokio::test]
async fn an_unconfirmed_check_parks_the_task_with_its_own_message() {
    let env = env().await;
    fs::write(env.dir.join("mode"), "refuse").unwrap();
    let yaml = gated("    claude_settings:\n      attribution: {commit: c}\n");
    let task = env.create(&yaml, json!({})).await.unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    wait_until_task_status(&env.pool, &task.id, "stuck").await;

    let after = env.task(&task.id).await;
    assert_eq!(
        after.stuck_reason.as_deref(),
        Some(
            "stage 'coding': choco could not confirm that Claude Code applied role 'coder''s claude_settings, so it stopped the turn; the session's error event says why; 'choco task retry' tries again"
        )
    );
    let rows = sessions::list_for_task(&env.pool, &task.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].end_reason,
        Some(SessionEndReason::ClaudeSettingsUnconfirmed)
    );
    assert!(!env.prompt_written());
    let text = all_event_text(&env.pool, &task.id).await;
    assert!(
        text.contains("(its tool server reported: the CLI refused the mcp_status request)"),
        "{text}"
    );
    assert!(!text.contains("CHOCOFACTORY_SETTINGS_"), "{text}");
    assert!(!text.contains("Unsupported control request"), "{text}");
}
