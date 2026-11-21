//! `kind: open_review`: a shell stage whose validated stdout is the task's
//! review. These run real scripts in real process groups under the machine's
//! temporary directory; nothing here starts an agent or the live daemon.

use super::stage_results::{ask, by_name, found, gate_flow_from, result_of};
use super::*;
use chocofactory_core::context::{ContextResponse, UnavailableReason};
use chocofactory_core::models::{Event, REVIEW_STAGE_KEY, ReviewRef};

/// A workflow whose only stage is a publisher (`publish`), for the engine's
/// kind classification.
pub(super) const PUBLISHER_FLOW: &str = r#"
name: review-flow
stages:
  publish:
    kind: open_review
    command: "true"
    on: { done: finished }
  finished:
    kind: terminal
"#;

const GITHUB_7: &str = r#"printf '{"number":7,"url":"https://github.com/o/r/pull/7"}'"#;

/// A task on a workflow written under a fresh temporary directory, which is
/// also the task's working directory and the base for `script_file`.
struct Flow {
    pool: SqlitePool,
    engine: Arc<WorkflowEngine>,
    def: Arc<WorkflowDefinition>,
    task_id: String,
    dir: TempDir,
}

impl Flow {
    async fn new(yaml: &str) -> Flow {
        let dir = tempdir();
        // Loading checks that every `script_file` exists and is executable:
        // leave an empty one for each, which `script` then fills in.
        for name in yaml
            .lines()
            .filter_map(|l| l.trim().strip_prefix("script_file: "))
        {
            crate::test_support::write_executable(&dir.join(name), "#!/bin/sh\n");
        }
        let def_name = yaml
            .lines()
            .find_map(|l| l.strip_prefix("name: "))
            .unwrap()
            .to_string();
        fs::write(dir.join(format!("{def_name}.yaml")), yaml).unwrap();
        let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task_in(&pool, &def.name, &dir).await;
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
        Flow {
            pool,
            engine,
            def,
            task_id,
            dir,
        }
    }

    /// A single-publisher flow: `publish` runs the inline `command`, with
    /// `on` as its edges, then `finished` (terminal) or `failed` (a gate).
    async fn publisher(command: &str, on: &str) -> Flow {
        let command = command
            .lines()
            .map(|l| format!("      {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        Flow::new(&format!(
            "name: review-flow\nstages:\n  publish:\n    kind: open_review\n    command: |\n{command}\n    on: {on}\n  finished:\n    kind: terminal\n  failed:\n    kind: human_gate\n    on: {{ resumed: finished }}\n"
        ))
        .await
    }

    async fn start(&self) {
        self.engine
            .start_task(&self.task_id, &self.def, None)
            .await
            .unwrap();
    }

    fn script(&self, name: &str, body: &str) {
        crate::test_support::write_executable(&self.dir.join(name), format!("#!/bin/sh\n{body}"));
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.dir.join(name), text).unwrap();
    }

    async fn results(&self) -> Vec<Event> {
        stage_results(&self.pool, &self.task_id).await
    }

    async fn state(&self) -> chocofactory_core::models::WorkflowState {
        state_of(&self.pool, &self.task_id).await
    }

    async fn payload(&self) -> Value {
        self.state().await.payload
    }

    async fn wait_stage(&self, stage: &str) {
        wait_until_stage(&self.pool, &self.task_id, stage).await;
    }

    /// Waits for the task's first departure, wherever it led.
    async fn wait_first_result(&self) {
        crate::test_support::wait_until("the first stage result", || async {
            if self.results().await.is_empty() {
                Err("none yet".to_string())
            } else {
                Ok(())
            }
        })
        .await
    }

    async fn wait_status(&self, status: &str) {
        wait_until_task_status(&self.pool, &self.task_id, status).await;
    }

    /// Waits for the departures of one lap to be committed, with the task
    /// parked at `stage`: a result and its move are one transaction, so both
    /// are seen together or not at all.
    async fn wait_settled(&self, results: usize, stage: &str) {
        crate::test_support::wait_until(
            &format!("{results} results with the task at '{stage}'"),
            || async {
                let have = self.results().await.len();
                let at = self.state().await.current_stage;
                if have == results && at == stage {
                    Ok(())
                } else {
                    Err(format!("{have} results, at '{at}'"))
                }
            },
        )
        .await
    }

    /// The task's reviews as the list and as the detail report them; the two
    /// must agree, and only that one answer is returned.
    async fn reviews(&self) -> Vec<ReviewRef> {
        let rows = crate::db::tasks::list_summaries(
            &self.pool,
            None,
            &[],
            crate::db::tasks::SummaryOrder::Id,
            None,
        )
        .await
        .unwrap();
        let listed = rows
            .into_iter()
            .find(|row| row.task.id == self.task_id)
            .unwrap()
            .reviews;
        let detail = crate::api::tasks::reviews_of(&self.payload().await);
        assert_eq!(listed, detail, "list and detail disagree");
        listed
    }

    /// What the publisher's command printed about itself on the timeline.
    async fn command_notes(&self) -> Vec<Option<String>> {
        events::list_for_task(&self.pool, &self.task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::ShellOutput)
            .map(|e| e.payload["note"].as_str().map(str::to_string))
            .collect()
    }

    async fn entered(&self) -> Vec<Value> {
        events::list_for_task(&self.pool, &self.task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == EventType::StageEntered)
            .map(|e| e.payload)
            .collect()
    }

    /// The stages entered, oldest first.
    async fn trail(&self) -> Vec<String> {
        self.entered()
            .await
            .iter()
            .map(|p| p["stage"].as_str().unwrap().to_string())
            .collect()
    }
}

impl Drop for Flow {
    /// A failed assertion must not leave a command running: abort every
    /// runner, which drops its process-group guard.
    fn drop(&mut self) {
        let handles: Vec<_> = self
            .engine
            .detached_runners
            .lock()
            .unwrap()
            .drain()
            .flat_map(|(_, task)| task.into_values().filter_map(|(_, handle)| handle))
            .collect();
        for handle in handles {
            handle.abort();
        }
    }
}

fn no_publication(payload: &Value) {
    assert!(
        payload.get(REVIEW_STAGE_KEY).is_none(),
        "unexpected pointer: {payload}"
    );
    assert!(
        payload["stages"].get("publish").is_none(),
        "unexpected capture: {payload}"
    );
}

fn github(number: u64) -> ReviewRef {
    ReviewRef {
        stage: "publish".into(),
        number,
        url: format!("https://github.com/o/r/pull/{number}"),
        label: format!("#{number}"),
    }
}

// ---- rejection routing ----------------------------------------------

#[tokio::test]
async fn invalid_publisher_output_follows_the_error_edge_and_publishes_nothing() {
    let f = Flow::publisher("echo 'opened it'", "{ done: finished, error: failed }").await;
    f.start().await;
    f.wait_first_result().await;

    let results = f.results().await;
    assert_eq!(results.len(), 1, "{results:?}");
    let result = result_of(&results[0]);
    assert_eq!(result.outcome, "error");
    assert_eq!(result.source_kind, "open_review");
    assert_eq!(result.destination_stage, "failed");
    assert!(!result.capture_present);
    no_publication(&f.payload().await);
    assert!(f.reviews().await.is_empty());
    let notes = f.command_notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    let note = notes[0].as_deref().unwrap();
    assert!(
        note.starts_with("open_review output rejected: output must be valid JSON"),
        "{note}"
    );
}

#[tokio::test]
async fn rejected_publisher_output_without_an_error_edge_parks_the_task_stuck() {
    let f = Flow::publisher(
        r#"printf '{"number":0,"url":"https://github.com/o/r/pull/0"}'"#,
        "{ done: finished }",
    )
    .await;
    f.start().await;
    f.wait_status("stuck").await;

    // Still at the publisher, with nothing recorded as having happened.
    assert_eq!(f.state().await.current_stage, "publish");
    assert!(f.results().await.is_empty());
    no_publication(&f.payload().await);
    assert_eq!(f.trail().await, ["publish"]);
    let notes = f.command_notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0]
            .as_deref()
            .unwrap()
            .starts_with("open_review output rejected: `number` must be a positive integer"),
        "{notes:?}"
    );
    let task = tasks::get(&f.pool, &f.task_id).await.unwrap().unwrap();
    let reason = task.stuck_reason.unwrap();
    assert!(
        reason.contains("has no 'on:' edge for it") && reason.contains("'error'"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_publisher_that_exits_nonzero_with_valid_output_is_a_plain_command_failure() {
    let f = Flow::publisher(
        &format!("{GITHUB_7}\nexit 1"),
        "{ done: finished, error: failed }",
    )
    .await;
    f.start().await;
    f.wait_stage("failed").await;

    let result = result_of(&f.results().await[0]);
    assert_eq!(result.outcome, "error");
    assert!(!result.capture_present);
    no_publication(&f.payload().await);
    // Today's failure note, not an output rejection.
    assert_eq!(
        f.command_notes().await,
        [Some(
            "stdout not captured: the command did not succeed".to_string()
        )]
    );
}

#[tokio::test]
async fn a_publisher_killed_by_a_signal_or_missing_at_spawn_publishes_nothing() {
    // Killed by a signal after printing a valid reference.
    let f = Flow::publisher(
        &format!("{GITHUB_7}\nkill -TERM $$"),
        "{ done: finished, error: failed }",
    )
    .await;
    f.start().await;
    f.wait_stage("failed").await;
    let result = result_of(&f.results().await[0]);
    assert_eq!(
        (result.outcome.as_str(), result.capture_present),
        ("error", false)
    );
    no_publication(&f.payload().await);
    assert!(f.reviews().await.is_empty());

    // An executable script removed after the definition loaded.
    let dir = tempdir();
    let yaml = "name: review-flow\nstages:\n  publish:\n    kind: open_review\n    script_file: publish.sh\n    on: { done: finished, error: failed }\n  finished:\n    kind: terminal\n  failed:\n    kind: human_gate\n    on: { resumed: finished }\n";
    crate::test_support::write_executable(
        &dir.join("publish.sh"),
        format!("#!/bin/sh\n{GITHUB_7}\n"),
    );
    fs::write(dir.join("review-flow.yaml"), yaml).unwrap();
    let def = Arc::new(WorkflowDefinition::parse(yaml, &dir).unwrap());
    fs::remove_file(dir.join("publish.sh")).unwrap();
    let pool = connect_in_memory().await.unwrap();
    let task_id = seed_task_in(&pool, &def.name, &dir).await;
    let engine = engine_with_adapter_and_workflows_dir(pool.clone(), "unused", &dir);
    engine.start_task(&task_id, &def, None).await.unwrap();
    wait_until_stage(&pool, &task_id, "failed").await;
    let result = result_of(&stage_results(&pool, &task_id).await[0]);
    assert_eq!(
        (result.outcome.as_str(), result.capture_present),
        ("error", false)
    );
    let payload = payload_of(&pool, &task_id).await;
    no_publication(&payload);
}

// ---- laps ---------------------------------------------------------------

const LAP_FLOW: &str = r#"name: review-laps
stages:
  publish:
    kind: open_review
    script_file: publish.sh
    timeout: 10s
    on: { done: after, error: gate }
  after:
    kind: shell
    command: |
      printf '{"number":9,"url":"https://github.com/o/r/pull/9"}'
    capture: json
    on: { done: gate }
  gate:
    kind: human_gate
    on: { resumed: publish, close: finished }
  finished:
    kind: terminal
"#;

/// The publisher's modes come from the file `mode`: `ok <n>`, `fail`, or
/// `hang <tag>`, which records its pids under `<tag>.*` and then a
/// `<tag>.started` marker, and sleeps in its process group.
const LAP_SCRIPT: &str = r#"mode=$(cat mode)
case "$mode" in
  "ok "*) n=${mode#ok }; printf '{"number":%s,"url":"https://github.com/o/r/pull/%s"}' "$n" "$n" ;;
  fail) printf '{"number":99,"url":"https://github.com/o/r/pull/99"}'; exit 1 ;;
  "hang "*) tag=${mode#hang }; echo $$ > "$tag.script"; sleep 600 & echo $! > "$tag.sleeper"; : > "$tag.started"; wait ;;
esac
"#;

/// `ps -o <field>= -p <pid>`: the OS's own answer, independent of the engine.
fn ps(field: &str, pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap().trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn pid_file(f: &Flow, name: &str) -> u32 {
    fs::read_to_string(f.dir.join(name))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Waits for a hang lap's started marker, then checks with `ps` that its
/// sleeper runs in the publisher script's own process group. Returns the
/// (script, sleeper) pids.
async fn hang_started(f: &Flow, tag: &str) -> (u32, u32) {
    let marker = f.dir.join(format!("{tag}.started"));
    crate::test_support::wait_until(&format!("{tag} started"), || async {
        marker.exists().then_some(()).ok_or("not yet".to_string())
    })
    .await;
    let (script, sleeper) = (
        pid_file(f, &format!("{tag}.script")),
        pid_file(f, &format!("{tag}.sleeper")),
    );
    assert!(process_alive(script) && process_alive(sleeper));
    assert_eq!(ps("pgid", sleeper), Some(script.to_string()), "same group");
    assert_eq!(
        ps("pgid", script),
        Some(script.to_string()),
        "its own group"
    );
    (script, sleeper)
}

async fn assert_group_gone(script: u32, sleeper: u32) {
    crate::test_support::wait_until("the process group to die", || async {
        if process_alive(script) || process_alive(sleeper) {
            Err("still alive".to_string())
        } else {
            Ok(())
        }
    })
    .await;
    assert_eq!((ps("pid", script), ps("pid", sleeper)), (None, None));
}

#[tokio::test]
async fn the_review_is_the_last_successful_publication_through_failed_timed_out_and_cancelled_laps()
{
    let f = Flow::new(LAP_FLOW).await;
    f.script("publish.sh", LAP_SCRIPT);
    f.write("mode", "fail");
    f.start().await;

    // A failed first lap: nothing is published, though it printed a valid object.
    f.wait_settled(1, "gate").await;
    let payload = f.payload().await;
    no_publication(&payload);
    assert!(f.reviews().await.is_empty());

    // ok 1 — and `after` captures an unrelated number and URL of its own.
    f.write("mode", "ok 1");
    f.engine
        .advance(&f.task_id, &f.def, "resumed")
        .await
        .unwrap();
    f.wait_settled(4, "gate").await;
    assert_eq!(f.reviews().await, [github(1)]);
    let payload = f.payload().await;
    assert_eq!(payload["stages"]["after"]["number"], 9);
    assert_eq!(payload[REVIEW_STAGE_KEY], "publish");
    let published = payload["stages"]["publish"].clone();

    // A failed lap keeps #1; its result is an error with no capture.
    f.write("mode", "fail");
    f.engine
        .advance(&f.task_id, &f.def, "resumed")
        .await
        .unwrap();
    f.wait_settled(6, "gate").await;
    assert_eq!(f.reviews().await, [github(1)]);
    let lap = result_of(&f.results().await[5]);
    assert_eq!(
        (lap.outcome.as_str(), lap.capture_present),
        ("error", false)
    );
    assert_eq!(f.payload().await["stages"]["publish"], published);

    // A hung lap killed by the timeout keeps #1 too, and its group is gone.
    f.write("mode", "hang t1");
    f.engine
        .advance(&f.task_id, &f.def, "resumed")
        .await
        .unwrap();
    let (script, sleeper) = hang_started(&f, "t1").await;
    assert_eq!(f.reviews().await, [github(1)]);
    f.wait_settled(8, "gate").await;
    assert_eq!(f.reviews().await, [github(1)]);
    let lap = result_of(&f.results().await[7]);
    assert_eq!(
        (lap.outcome.as_str(), lap.capture_present),
        ("error", false)
    );
    assert_group_gone(script, sleeper).await;

    // ok 2 replaces the snapshot.
    f.write("mode", "ok 2");
    f.engine
        .advance(&f.task_id, &f.def, "resumed")
        .await
        .unwrap();
    f.wait_settled(11, "gate").await;
    assert_eq!(f.reviews().await, [github(2)]);
    let published = f.payload().await["stages"]["publish"].clone();

    // A lap cancelled while it runs keeps #2, and records nothing.
    f.write("mode", "hang t2");
    f.engine
        .advance(&f.task_id, &f.def, "resumed")
        .await
        .unwrap();
    let (script, sleeper) = hang_started(&f, "t2").await;
    assert_eq!(f.reviews().await, [github(2)]);
    assert_eq!(f.results().await.len(), 12);
    f.engine.cancel_task(&f.task_id, false).await.unwrap();
    assert_eq!(f.reviews().await, [github(2)]);
    assert_eq!(f.results().await.len(), 12, "no further departure");
    assert_eq!(f.payload().await["stages"]["publish"], published);
    assert_group_gone(script, sleeper).await;

    // Kinds, everywhere the name is recorded.
    for result in f.results().await {
        let result = result_of(&result);
        if result.source_stage == "publish" {
            assert_eq!(result.source_kind, "open_review", "{result:?}");
        }
    }
    let entered = f.entered().await;
    for entry in &entered {
        let stage = entry["stage"].as_str().unwrap();
        let kind = match stage {
            "publish" => "open_review",
            "after" => "shell",
            _ => "human_gate",
        };
        assert_eq!(entry["kind"], kind, "{entry}");
    }
    assert!(entered.iter().any(|e| e["stage"] == "publish"));
    // The cancelled lap left the task at the publisher.
    assert_eq!(f.state().await.current_stage, "publish");
    assert_eq!(f.state().await.stage_kind.as_deref(), Some("open_review"));
}

#[tokio::test]
async fn a_failed_lap_leaves_the_previous_publication_exactly_as_it_was() {
    let f = Flow::new(
        r#"name: review-failing
stages:
  publish:
    kind: open_review
    script_file: publish.sh
    on: { done: finished, error: failed }
  finished:
    kind: terminal
  failed:
    kind: human_gate
    on: { resumed: finished }
"#,
    )
    .await;
    // Waits for `go`, then prints another valid reference and fails.
    f.script(
        "publish.sh",
        "while [ ! -f go ]; do sleep 0.05; done\nprintf '{\"number\":99,\"url\":\"https://github.com/o/r/pull/99\"}'\nexit 1\n",
    );
    f.start().await;
    // An earlier lap published #1.
    let mut state = f.state().await;
    state.payload[REVIEW_STAGE_KEY] = json!("publish");
    state.payload["stages"] =
        json!({"publish": {"number": 1, "url": "https://github.com/o/r/pull/1", "label": "#1"}});
    workflow_state::update(
        &f.pool,
        &f.task_id,
        workflow_state::WorkflowStateUpdate {
            current_stage: state.current_stage,
            stage_kind: state.stage_kind.unwrap(),
            loop_counters: state.loop_counters,
            payload: state.payload.clone(),
            enters_stage: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(f.reviews().await, [github(1)]);

    f.write("go", "");
    f.wait_first_result().await;
    // The lap is an error with no capture, and #1 is still the review.
    let lap = result_of(&f.results().await[0]);
    assert_eq!(
        (lap.outcome.as_str(), lap.capture_present),
        ("error", false)
    );
    assert_eq!(f.reviews().await, [github(1)]);
    let after = f.payload().await;
    assert_eq!(after["stages"], state.payload["stages"]);
    assert_eq!(after[REVIEW_STAGE_KEY], "publish");
}

#[tokio::test]
async fn github_and_self_hosted_publications_read_back_identically_in_list_and_detail() {
    for (printed, expected) in [
        (
            r#"{"number":7,"url":"https://github.com/o/r/pull/7"}"#,
            ReviewRef {
                stage: "publish".into(),
                number: 7,
                url: "https://github.com/o/r/pull/7".into(),
                label: "#7".into(),
            },
        ),
        (
            r#"{"number":42,"url":"https://git.example.internal/group/sub/proj/-/merge_requests/42","label":"!42"}"#,
            ReviewRef {
                stage: "publish".into(),
                number: 42,
                url: "https://git.example.internal/group/sub/proj/-/merge_requests/42".into(),
                label: "!42".into(),
            },
        ),
    ] {
        let f = Flow::publisher(&format!("printf '%s' '{printed}'"), "{ done: finished }").await;
        f.start().await;
        f.wait_stage("finished").await;
        assert_eq!(f.reviews().await, [expected]);
    }
}

#[tokio::test]
async fn a_later_stage_templates_the_publishers_fields_through_env() {
    let f = Flow::new(
        r#"name: review-template
stages:
  publish:
    kind: open_review
    command: |
      printf '{"number":7,"url":"https://github.com/o/r/pull/7","extra":"x"}'
    on: { done: render }
  render:
    kind: shell
    command: 'printf "%s %s" "$R_LABEL" "$R_EXTRA"'
    env:
      R_LABEL: "{{ stages.publish.label }}"
      R_EXTRA: "{{ stages.publish.extra }}"
    capture: text
    on: { done: finished }
  finished:
    kind: terminal
"#,
    )
    .await;
    f.start().await;
    f.wait_stage("finished").await;
    let payload = f.payload().await;
    assert_eq!(payload["stages"]["render"], "#7 x");
    // Extra fields survive, and the label was filled in.
    assert_eq!(
        payload["stages"]["publish"],
        json!({"number": 7, "url": "https://github.com/o/r/pull/7", "extra": "x", "label": "#7"})
    );
}

// ---- atomic departure ---------------------------------------------------

/// A publisher that waits for the file `go` and is the entry stage, then a
/// gate `hold`.
async fn gated_publisher() -> Flow {
    let f = Flow::new(
        r#"name: review-atomic
stages:
  publish:
    kind: open_review
    script_file: publish.sh
    on: { done: hold }
  hold:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#,
    )
    .await;
    f.script(
        "publish.sh",
        "while [ ! -f go ]; do sleep 0.05; done\nprintf '{\"number\":5,\"url\":\"https://github.com/o/r/pull/5\"}'\n",
    );
    f.start().await;
    f
}

/// Everything a departure writes or moves.
async fn departure_facts(
    f: &Flow,
) -> (
    chocofactory_core::models::WorkflowState,
    Vec<Event>,
    Vec<String>,
) {
    (f.state().await, f.results().await, f.trail().await)
}

async fn assert_departure_refused(f: &Flow, trigger: &str) {
    let before = departure_facts(f).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(trigger.to_string()))
        .execute(&f.pool)
        .await
        .unwrap();
    f.write("go", "");
    f.wait_status("stuck").await;

    let after = departure_facts(f).await;
    assert!(
        after.0.payload.get(REVIEW_STAGE_KEY).is_none(),
        "no review pointer: {}",
        after.0.payload
    );
    assert!(
        after.0.payload["stages"].get("publish").is_none(),
        "no publisher capture"
    );
    assert_eq!(after.1, before.1, "no stage result");
    assert_eq!(after.0.current_stage, "publish");
    assert_eq!(after.2, before.2, "no destination entered");
    assert_eq!(after, before, "nothing about the departure was recorded");
    assert!(f.reviews().await.is_empty());
    let task = tasks::get(&f.pool, &f.task_id).await.unwrap().unwrap();
    assert!(
        task.stuck_reason
            .unwrap()
            .contains("command completed but the transition failed"),
    );
}

#[tokio::test]
async fn a_refused_result_insert_publishes_nothing_and_enters_no_destination() {
    let f = gated_publisher().await;
    assert_departure_refused(
        &f,
        "CREATE TRIGGER no_results BEFORE INSERT ON events
         WHEN NEW.event_type = 'stage_result'
         BEGIN SELECT RAISE(ABORT, 'insert refused'); END",
    )
    .await;
}

#[tokio::test]
async fn a_refused_state_update_publishes_nothing_and_leaves_no_orphan_result() {
    let f = gated_publisher().await;
    assert_departure_refused(
        &f,
        "CREATE TRIGGER no_state_moves BEFORE UPDATE ON workflow_state
         BEGIN SELECT RAISE(ABORT, 'update refused'); END",
    )
    .await;
}

#[tokio::test]
async fn review_departure_commit_failure_rolls_back_every_fact() {
    let f = gated_publisher().await;
    // The insert and the update both run; a deferred foreign key then fails
    // the COMMIT itself.
    assert_departure_refused(
        &f,
        "CREATE TABLE commit_parent (id TEXT PRIMARY KEY);
         CREATE TABLE commit_probe (parent TEXT REFERENCES commit_parent (id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER commit_fails AFTER UPDATE ON workflow_state
         BEGIN INSERT INTO commit_probe VALUES ('no such parent'); END",
    )
    .await;
    let probes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM commit_probe")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(probes, 0, "the trigger's row was rolled back too");
}

// ---- history ------------------------------------------------------------

const CHAIN_FLOW: &str = r#"name: sr-gate
stages:
  gate:
    kind: human_gate
    capture: text
    markers:
      - line: /go
        then: go
    on: { go: publish }
  publish:
    kind: open_review
    command: |
      printf '{"number":12,"url":"https://git.example.internal/g/p/-/merge_requests/12","label":"!12"}'
    on: { done: hold }
  hold:
    kind: human_gate
    on: { resumed: finished }
  finished:
    kind: terminal
"#;

#[tokio::test]
async fn a_lookup_walks_back_through_the_publication_to_the_gate_before_it() {
    let f = gate_flow_from(CHAIN_FLOW, connect_in_memory().await.unwrap(), tempdir()).await;
    f.reply("please fix the parser\n/go").await.unwrap();
    wait_until_stage(&f.pool, &f.task_id, "hold").await;

    let gate = found(ask(&f.pool, &f.task_id, by_name("gate", None, None)).await);
    assert_eq!(gate.source_kind, "human_gate");
    assert_eq!(gate.capture, json!("please fix the parser"));
    let publish = found(ask(&f.pool, &f.task_id, by_name("publish", None, None)).await);
    assert_eq!(publish.source_kind, "open_review");
    assert_eq!(publish.capture["label"], "!12");
}

#[tokio::test]
async fn pruning_the_publications_event_keeps_the_review_and_reports_a_history_gap() {
    let f = gate_flow_from(CHAIN_FLOW, connect_in_memory().await.unwrap(), tempdir()).await;
    f.reply("please fix the parser\n/go").await.unwrap();
    wait_until_stage(&f.pool, &f.task_id, "hold").await;
    f.engine
        .advance(&f.task_id, &f.def, "resumed")
        .await
        .unwrap();

    let results = f.results().await;
    assert_eq!(results.len(), 3);
    let publication = result_of(&results[1]);
    assert_eq!(publication.source_stage, "publish");
    let expected = ReviewRef {
        stage: "publish".into(),
        number: 12,
        url: "https://git.example.internal/g/p/-/merge_requests/12".into(),
        label: "!12".into(),
    };
    let reviews_now = || async {
        let rows = crate::db::tasks::list_summaries(
            &f.pool,
            None,
            &[],
            crate::db::tasks::SummaryOrder::Id,
            None,
        )
        .await
        .unwrap();
        let listed = rows.into_iter().next().unwrap().reviews;
        assert_eq!(
            listed,
            crate::api::tasks::reviews_of(&f.state().await.payload)
        );
        listed
    };
    assert_eq!(reviews_now().await, std::slice::from_ref(&expected));

    // Only the publication's event is old; the later anchor points through it.
    sqlx::query("UPDATE events SET created_at = '2000-01-01T00:00:00+00:00' WHERE id = ?")
        .bind(&publication.result_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let removed =
        events::delete_older_than(&f.pool, chrono::Utc::now() - chrono::Duration::days(1))
            .await
            .unwrap();
    assert_eq!(removed, 1);
    let remaining: Vec<String> = f.results().await.into_iter().map(|e| e.id).collect();
    assert!(!remaining.contains(&publication.result_id), "{remaining:?}");
    assert_eq!(remaining.len(), 2);

    assert_eq!(reviews_now().await, [expected]);
    match ask(&f.pool, &f.task_id, by_name("gate", None, None)).await {
        ContextResponse::Unavailable { reason, .. } => {
            assert_eq!(reason, UnavailableReason::HistoryGap)
        }
        other => panic!("expected a history gap, got {other:?}"),
    }
}

// ---- late outcomes --------------------------------------------------------

fn valid_late_capture() -> Value {
    json!({"number": 2, "url": "https://github.com/o/r/pull/2", "label": "#2"})
}

#[tokio::test]
async fn a_publishers_late_outcome_is_refused_once_the_task_is_cancelled() {
    let f = Flow::new(
        r#"name: review-late
stages:
  publish:
    kind: open_review
    script_file: publish.sh
    on: { done: finished }
  finished:
    kind: terminal
"#,
    )
    .await;
    f.script(
        "publish.sh",
        ": > started\nwhile [ ! -f go ]; do sleep 0.05; done\n",
    );
    f.start().await;
    let started = f.dir.join("started");
    crate::test_support::wait_until("the publisher to start", || async {
        started.exists().then_some(()).ok_or("not yet".to_string())
    })
    .await;
    f.engine.cancel_task(&f.task_id, false).await.unwrap();
    let before = (f.payload().await, f.results().await, f.trail().await);

    let late = f
        .engine
        .advance_from_stage(
            &f.task_id,
            &f.def,
            "done",
            Some("publish"),
            Some(valid_late_capture()),
            false,
        )
        .await;
    assert!(
        matches!(late, Err(EngineError::TaskCancelled(_))),
        "{late:?}"
    );
    assert_eq!(
        (f.payload().await, f.results().await, f.trail().await),
        before
    );
    assert!(f.reviews().await.is_empty());
}

#[tokio::test]
async fn a_publishers_late_outcome_is_refused_once_the_task_has_moved_on() {
    let f = Flow::new(
        r#"name: review-moved
stages:
  publish:
    kind: open_review
    command: |
      printf '{"number":1,"url":"https://github.com/o/r/pull/1"}'
    on: { done: block }
  block:
    kind: shell
    script_file: block.sh
    on: { done: finished }
  finished:
    kind: terminal
"#,
    )
    .await;
    f.script(
        "block.sh",
        ": > blocked\nwhile [ ! -f go ]; do sleep 0.05; done\n",
    );
    f.start().await;
    f.wait_stage("block").await;
    let blocked = f.dir.join("blocked");
    crate::test_support::wait_until("the next stage's command", || async {
        blocked.exists().then_some(()).ok_or("not yet".to_string())
    })
    .await;
    f.wait_settled(1, "block").await;
    assert_eq!(f.reviews().await, [github(1)]);
    let before = (f.payload().await, f.results().await, f.trail().await);

    let late = f
        .engine
        .advance_from_stage(
            &f.task_id,
            &f.def,
            "done",
            Some("publish"),
            Some(valid_late_capture()),
            false,
        )
        .await;
    assert!(
        matches!(&late, Err(EngineError::StageMovedOn { expected, actual })
            if expected == "publish" && actual == "block"),
        "{late:?}"
    );
    assert_eq!(
        (f.payload().await, f.results().await, f.trail().await),
        before,
        "the late outcome changed nothing"
    );
    assert_eq!(f.reviews().await, [github(1)]);
}

// ---- restart ------------------------------------------------------------------

#[tokio::test]
async fn an_interrupted_publisher_parks_and_retry_reuses_the_remote_review_it_already_made() {
    let f = Flow::new(
        r#"name: review-restart
stages:
  publish:
    kind: open_review
    script_file: publish.sh
    on: { done: finished }
  finished:
    kind: terminal
"#,
    )
    .await;
    // The remote write happens once, whatever the lap; a lap with `hang`
    // present then stops before reporting it.
    f.script(
        "publish.sh",
        r#"if [ ! -f remote.json ]; then
  printf '{"number":4242,"url":"https://gitlab.example.internal/g/p/-/merge_requests/4242","label":"!4242"}' > remote.tmp
  mv remote.tmp remote.json
  echo created >> remote.log
fi
if [ -f hang ]; then
  echo $$ > script.pid; sleep 600 & echo $! > sleeper.pid; : > started; wait
fi
cat remote.json
"#,
    );
    f.write("hang", "");
    f.start().await;
    let started = f.dir.join("started");
    crate::test_support::wait_until("the publisher to start", || async {
        started.exists().then_some(()).ok_or("not yet".to_string())
    })
    .await;
    let remote = f.dir.join("remote.json");
    let original = (
        fs::read_to_string(&remote).unwrap(),
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(&remote).unwrap()),
    );
    let (script, sleeper) = (pid_file(&f, "script.pid"), pid_file(&f, "sleeper.pid"));
    assert_eq!(ps("pgid", sleeper), Some(script.to_string()));

    // The daemon stops: the owned process group goes with it.
    f.engine.abort_all_detached_runners().await;
    assert_group_gone(script, sleeper).await;

    // A restart parks the task at the publisher; nothing was recorded.
    let engine2 = engine_with_adapter_and_workflows_dir(f.pool.clone(), "unused", &f.dir);
    engine2.park_interrupted_turns().await.unwrap();
    let task = tasks::get(&f.pool, &f.task_id).await.unwrap().unwrap();
    assert_eq!(task.status, "stuck");
    assert_eq!(
        task.stuck_reason.unwrap(),
        "stage 'publish' was running a shell command when the daemon stopped; 'choco task \
         retry' runs it again from the start"
    );
    no_publication(&f.payload().await);
    assert!(f.results().await.is_empty());

    // The stopped engine refuses new work; the new one retries.
    assert!(!f.engine.spawn_registered_runner(
        &f.task_id,
        runners::DetachedKind::Shell,
        |_| async {}
    ));
    fs::remove_file(f.dir.join("hang")).unwrap();
    engine2
        .retry_task(&f.task_id, RetryMode::Auto)
        .await
        .unwrap();
    f.wait_stage("finished").await;

    let expected = ReviewRef {
        stage: "publish".into(),
        number: 4242,
        url: "https://gitlab.example.internal/g/p/-/merge_requests/4242".into(),
        label: "!4242".into(),
    };
    assert_eq!(f.reviews().await, [expected]);
    assert_eq!(
        (
            fs::read_to_string(&remote).unwrap(),
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&remote).unwrap())
        ),
        original,
        "the remote review is the one the first lap made"
    );
    assert_eq!(
        fs::read_to_string(f.dir.join("remote.log")).unwrap(),
        "created\n"
    );
    let results = f.results().await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(result_of(&results[0]).outcome, "done");
    drop(engine2);
}
