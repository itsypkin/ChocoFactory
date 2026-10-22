//! A Claude role's `:<level>` model suffix (#336): where a bad one is refused
//! at workflow load, task create, config patch and turn start. Each site has
//! a mixed case: a valid role beside the invalid one, and the error names
//! only the invalid one.

use super::*;
use crate::adapter::OmpAdapter;
use crate::db::{connect_in_memory, projects, sessions, tasks};

/// Two Claude roles: `coder` (model given) and `reviewer` (always valid).
fn flow_yaml(coder_cli: &str, coder_model: Option<&str>) -> String {
    let coder_model = coder_model
        .map(|m| format!("    model: \"{m}\"\n"))
        .unwrap_or_default();
    format!(
        "name: lvl\nworktree: false\nroles:\n  coder:\n    cli: {coder_cli}\n{coder_model}  reviewer:\n    cli: claude\n    model: claude-opus-5-5:max\nstages:\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: p.md\n    on: {{ done: review }}\n  review:\n    kind: agent_turn\n    role: reviewer\n    prompt_file: p.md\n    on: {{ done: finished }}\n  finished:\n    kind: terminal\n"
    )
}

fn write_flow(dir: &Path, file: &str, yaml: &str) -> PathBuf {
    fs::write(dir.join("p.md"), "do it").unwrap();
    let path = dir.join(file);
    fs::write(&path, yaml).unwrap();
    path
}

fn registry(dir: &Path) -> Registry {
    Registry::new(vec![
        Arc::new(ClaudeAdapter::with_binary(fixture_binary("fake_claude.py"))),
        Arc::new(OmpAdapter::with_binary("/nonexistent/omp", dir.join("omp"))),
    ])
}

#[test]
fn load_refuses_every_non_claude_level_and_accepts_the_five() {
    let dir = tempdir();
    let registry = registry(&dir);
    for suffix in ["off", "minimal", "auto", "inherit", "hihg", ""] {
        let path = write_flow(
            &dir,
            "bad.yaml",
            &flow_yaml("claude", Some(&format!("claude-opus-5-5:{suffix}"))),
        );
        let err = load_workflow_file(&path, &registry).unwrap_err();
        assert!(matches!(err, WorkflowDefError::RoleRejected(_)), "{err:?}");
        let message = err.to_string();
        assert!(message.contains("`coder`"), "{message}");
        assert!(
            message.contains(&format!("claude-opus-5-5:{suffix}")),
            "{message}"
        );
        assert!(!message.contains("reviewer"), "{message}");
    }
    for level in ["low", "medium", "high", "xhigh", "max"] {
        let path = write_flow(
            &dir,
            "ok.yaml",
            &flow_yaml("claude", Some(&format!("claude-opus-5-5:{level}"))),
        );
        assert!(load_workflow_file(&path, &registry).is_ok(), "{level}");
    }
    // omp judges its own model strings.
    let path = write_flow(
        &dir,
        "omp.yaml",
        &flow_yaml("omp", Some("claude-opus-5-5:minimal")),
    );
    assert!(load_workflow_file(&path, &registry).is_ok());
}

struct Env {
    dir: TempDir,
    pool: SqlitePool,
    project_id: String,
    engine: Arc<WorkflowEngine>,
    global: PathBuf,
}

async fn env() -> Env {
    let pool = connect_in_memory().await.unwrap();
    let dir = tempdir();
    let global = dir.join("config.yaml");
    let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
    let engine = engine_with_registry(pool.clone(), registry(&dir), &dir, Some(&global));
    Env {
        dir,
        pool,
        project_id,
        engine,
        global,
    }
}

impl Env {
    async fn create(&self, yaml: &str, config: serde_json::Value) -> Result<Task, CreateTaskError> {
        let flow = write_flow(&self.dir, "flow.yaml", yaml);
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
}

#[tokio::test]
async fn create_refuses_a_bad_level_in_the_task_config_and_creates_nothing() {
    let env = env().await;
    let yaml = flow_yaml("claude", Some("claude-opus-5-5"));
    let err = env
        .create(
            &yaml,
            json!({"roles": {"coder": {"model": "claude-opus-5-5:off"}}}),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::RoleRejected(_)), "{err:?}");
    let message = err.to_string();
    assert!(message.contains("`coder`") && message.contains("claude-opus-5-5:off"));
    assert!(!message.contains("reviewer"), "{message}");
    assert_eq!(env.task_count().await, 0);
}

#[tokio::test]
async fn create_refuses_a_pair_that_comes_from_different_layers() {
    let env = env().await;
    // Task layer picks the cli, the workflow supplies the model.
    let yaml = flow_yaml("omp", Some("openai-codex/gpt-5.6-terra:minimal"));
    let err = env
        .create(&yaml, json!({"roles": {"coder": {"cli": "claude"}}}))
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::RoleRejected(_)), "{err:?}");
    assert!(err.to_string().contains("`coder`"));
    assert_eq!(env.task_count().await, 0);

    // Global layer supplies the model of a role whose workflow has none.
    fs::write(
        &env.global,
        "roles:\n  coder:\n    model: claude-sonnet-5-5:inherit\n",
    )
    .unwrap();
    let err = env
        .create(&flow_yaml("claude", None), json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::RoleRejected(_)), "{err:?}");
    assert!(err.to_string().contains("claude-sonnet-5-5:inherit"));
    assert_eq!(env.task_count().await, 0);
}

#[tokio::test]
async fn create_accepts_an_omp_role_with_a_claude_looking_bad_suffix() {
    let env = env().await;
    // The omp binary doesn't exist, so the task may get stuck starting; the
    // create itself must not be refused for the role's model string.
    let result = env
        .create(
            &flow_yaml("claude", Some("claude-opus-5-5")),
            json!({"roles": {"coder": {"cli": "omp", "model": "claude-opus-5-5:minimal"}}}),
        )
        .await;
    assert!(
        !matches!(result, Err(CreateTaskError::RoleRejected(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn create_fails_closed_when_the_global_config_will_not_load() {
    let env = env().await;
    fs::write(&env.global, "roles: [this is: not valid").unwrap();
    let err = env
        .create(&flow_yaml("claude", Some("claude-opus-5-5")), json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, CreateTaskError::GlobalConfig(_)), "{err:?}");
    assert!(matches!(
        crate::api::ApiError::from(err),
        crate::api::ApiError::Internal(_)
    ));
    assert_eq!(env.task_count().await, 0);
}

/// A gate in front of an agent turn, so the global config can be edited
/// after create and before the turn.
const GATED: &str = "name: gated\nworktree: false\nroles:\n  coder:\n    cli: claude\nstages:\n  review:\n    kind: human_gate\n    on: { resumed: coding }\n  coding:\n    kind: agent_turn\n    role: coder\n    prompt_file: p.md\n    on: { done: finished }\n  finished:\n    kind: terminal\n";

#[tokio::test]
async fn a_bad_level_edited_in_after_create_parks_the_task_at_turn_start() {
    let env = env().await;
    fs::write(
        &env.global,
        "roles:\n  coder:\n    model: claude-sonnet-5-5:high\n",
    )
    .unwrap();
    let task = env.create(GATED, json!({})).await.unwrap();
    fs::write(
        &env.global,
        "roles:\n  coder:\n    model: claude-sonnet-5-5:auto\n",
    )
    .unwrap();
    let _ = env.engine.send_message_or_resume(&task.id, "go").await;
    let task = tasks::get(&env.pool, &task.id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains("`coder`") && reason.contains(":auto"),
        "{reason}"
    );
    assert!(
        sessions::list_for_task(&env.pool, &task.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn check_config_patch_checks_the_merged_config_for_the_roles_it_touches() {
    let env = env().await;
    let yaml = flow_yaml("omp", Some("p/m:minimal"));
    let task = env.create(&yaml, json!({})).await;
    let task_id = match task {
        Ok(task) => task.id,
        Err(CreateTaskError::Start { task_id, .. }) => task_id,
        Err(other) => panic!("{other:?}"),
    };
    // Setting only `cli: claude` merges with the workflow's omp-only level.
    let err = env
        .engine
        .check_config_patch(&task_id, &json!({"roles": {"coder": {"cli": "claude"}}}))
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigPatchError::Rejected(_)), "{err:?}");
    // A bad model alone.
    let err = env
        .engine
        .check_config_patch(
            &task_id,
            &json!({"roles": {"coder": {"model": "x:minimal", "cli": "claude"}}}),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigPatchError::Rejected(_)), "{err:?}");
    // A role the patch doesn't touch is not checked: the reviewer's level is
    // fine here, and the coder still holds its omp-only one.
    env.engine
        .check_config_patch(
            &task_id,
            &json!({"roles": {"reviewer": {"model": "claude-opus-5-5:low"}}}),
        )
        .await
        .unwrap();
    // A null-delete drops the override.
    env.engine
        .check_config_patch(&task_id, &json!({"roles": {"coder": {"model": null}}}))
        .await
        .unwrap();
    // A global config that won't load fails closed.
    fs::write(&env.global, "roles: [oops: not valid").unwrap();
    let err = env
        .engine
        .check_config_patch(&task_id, &json!({"roles": {"coder": {"cli": "claude"}}}))
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigPatchError::GlobalConfig(_)), "{err:?}");
}
