//! The two parked human gates (`escalate_to_human`, `blocker`) close a task
//! whose PR was merged by hand (#379).
//!
//! These tests run the shipped workflow files and the shipped
//! `scripts/await-merge.sh` through the engine, with a fake `gh` first on
//! `PATH`. Only the gates' poll interval is cut from 60s to the loader's 1s
//! floor in the loaded copy. Every variant has its own PR number, which the
//! fake `gh` uses to keep each variant's PR state apart, so the six
//! variants (three workflows, two gates) can run side by side.

use super::*;

/// One run of the shipped parked-gate wiring.
#[derive(Clone, Copy)]
struct Variant {
    workflow: &'static str,
    gate: &'static str,
    pr: u32,
}

impl Variant {
    fn label(&self) -> String {
        format!("{} at {} (PR {})", self.workflow, self.gate, self.pr)
    }

    /// The arrival a parked gate really has: the escalation of a failed
    /// `open_pr`, or the coder's `blocked` report.
    fn arrival(&self) -> Value {
        match self.gate {
            "blocker" => json!({ "from": "coding", "outcome": "blocked" }),
            _ => json!({ "from": "open_pr", "outcome": "error" }),
        }
    }
}

const VARIANTS: [Variant; 6] = [
    Variant {
        workflow: "coding-task",
        gate: "escalate_to_human",
        pr: 4101,
    },
    Variant {
        workflow: "coding-task",
        gate: "blocker",
        pr: 4102,
    },
    Variant {
        workflow: "coding-task-planned",
        gate: "escalate_to_human",
        pr: 4103,
    },
    Variant {
        workflow: "coding-task-planned",
        gate: "blocker",
        pr: 4104,
    },
    Variant {
        workflow: "review-panel",
        gate: "escalate_to_human",
        pr: 4105,
    },
    Variant {
        workflow: "review-panel",
        gate: "blocker",
        pr: 4106,
    },
];

/// The merge-state call the script makes, and nothing else. Any other call
/// is recorded in `unexpected` and fails, so a comment, review or conflict
/// read, or a `pr create`, cannot go unnoticed. The record behind each PR
/// is a JSON file the test owns; `.state` is applied with `jq`, which
/// stands in for the one embedded in `gh`.
const GH_STUB: &str = r#"#!/bin/sh
root="@ROOT@"
echo "$*" >> "$root/all-calls"
if [ "$#" -ne 7 ] || [ "$1" != pr ] || [ "$2" != view ] || [ "$4" != --json ] || [ "$5" != state ] || [ "$6" != -q ] || [ "$7" != .state ]; then
  echo "$*" >> "$root/unexpected"
  echo "stub gh: unexpected call: $*" >&2
  exit 99
fi
dir="$root/pr-$3"
if [ ! -d "$dir" ]; then
  echo "$*" >> "$root/unexpected"
  echo "stub gh: no such PR: $3" >&2
  exit 98
fi
if [ -e "$dir/fail" ]; then
  echo MERGED
  echo "stub gh: HTTP 502 while reading the pull request" >&2
  echo failed >> "$dir/served"
  exit 17
fi
out=$(jq -r "$7" < "$dir/record.json") || exit 1
printf '%s\n' "$out"
echo "$out" >> "$dir/served"
"#;

const OPEN_WITH_REVIEW_DATA: &str = r#"{"state":"OPEN","mergeable":"CONFLICTING","reviews":[{"state":"APPROVED","body":"/approve"},{"state":"CHANGES_REQUESTED","body":"/request-changes"}],"comments":[{"body":"/approve"},{"body":"/request-changes"}]}"#;
const CLOSED_UNMERGED: &str = r#"{"state":"CLOSED","mergedAt":null}"#;
const MERGED: &str = r#"{"state":"MERGED","mergedAt":"2026-11-14T14:43:38Z"}"#;
const FAILED_READ_NOTE: &str = "stub gh: HTTP 502 while reading the pull request";

struct GhStub {
    root: TempDir,
}

impl GhStub {
    fn new() -> Self {
        let root = tempdir();
        let script = GH_STUB.replace("@ROOT@", &root.to_string_lossy());
        write_script(&root, "gh", &script);
        // A `claude` that starts and then idles, for a test whose reply
        // enters the coder's stage: the turn starts, and a cancel ends it.
        write_script(&root, "claude", "#!/bin/sh\nsleep 120\n");
        GhStub { root }
    }

    fn claude(&self) -> String {
        self.root.join("claude").to_string_lossy().into_owned()
    }

    fn pr_dir(&self, pr: u32) -> PathBuf {
        self.root.join(format!("pr-{pr}"))
    }

    fn set_record(&self, pr: u32, json: &str) {
        fs::create_dir_all(self.pr_dir(pr)).unwrap();
        fs::write(self.pr_dir(pr).join("record.json"), json).unwrap();
    }

    fn set_failing(&self, pr: u32, failing: bool) {
        let marker = self.pr_dir(pr).join("fail");
        if failing {
            fs::write(marker, "").unwrap();
        } else {
            let _ = fs::remove_file(marker);
        }
    }

    /// What each completed read of `pr` answered, oldest first.
    fn served(&self, pr: u32) -> Vec<String> {
        fs::read_to_string(self.pr_dir(pr).join("served"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn all_calls(&self) -> String {
        fs::read_to_string(self.root.join("all-calls")).unwrap_or_default()
    }

    fn unexpected(&self) -> String {
        fs::read_to_string(self.root.join("unexpected")).unwrap_or_default()
    }
}

/// The call `await-merge.sh` makes for `pr`, as the stub logs it.
fn merge_probe_call(pr: u32) -> String {
    format!("pr view {pr} --json state -q .state\n")
}

fn copy_dereferenced(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if fs::metadata(entry.path()).unwrap().is_dir() {
            copy_dereferenced(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A directory holding `workflow` as shipped (seeded with its scripts, or
/// the experimental one copied out of the checkout), except that the two
/// parked gates poll every 1s instead of every 60s.
fn workflows_dir_with_fast_gates(workflow: &str) -> TempDir {
    let dir = tempdir();
    if workflow == "review-panel" {
        copy_dereferenced(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/experimental"),
            &dir,
        );
    } else {
        config_root::seed_builtin_workflows(&dir).unwrap();
    }
    let path = dir.join(format!("{workflow}.yaml"));
    let mut yaml = fs::read_to_string(&path).unwrap();
    for gate in ["escalate_to_human", "blocker"] {
        // Only inside the gate's own block: a gate without a watcher (a
        // mutated copy) has no interval to cut.
        let start = yaml.find(&format!("\n  {gate}:\n")).unwrap() + 1;
        let end = start + yaml[start..].find("\n\n").unwrap();
        if let Some(at) = yaml[start..end].find("interval: 60s") {
            let at = start + at;
            yaml.replace_range(at..at + "interval: 60s".len(), "interval: 1s");
        }
    }
    fs::write(&path, yaml).unwrap();
    dir
}

/// One task parked at a gate of a shipped workflow.
struct ParkedRig {
    label: String,
    pool: SqlitePool,
    engine: Arc<WorkflowEngine>,
    task_id: String,
    gate: &'static str,
    pr: u32,
    repo: TempDir,
    _workflows_dir: TempDir,
}

impl ParkedRig {
    /// Creates the task, with its real worktree, but does not enter the
    /// gate. Returns the rig and the loaded definition.
    async fn build(v: Variant, claude: &str) -> (ParkedRig, Arc<WorkflowDefinition>) {
        let workflows_dir = workflows_dir_with_fast_gates(v.workflow);
        let def = Arc::new(
            WorkflowDefinition::load(&workflows_dir.join(format!("{}.yaml", v.workflow))).unwrap(),
        );
        let pool = connect_in_memory().await.unwrap();
        let repo = tempdir();
        init_git_repo(&repo).await;
        let project_id = projects::create(&pool, "demo", None).await.unwrap().id;
        let task_id = tasks::create(
            &pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: &def.name,
                title: "Add a small feature",
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
        let engine = engine_with_adapter_and_workflows_dir(pool.clone(), claude, &workflows_dir);
        let rig = ParkedRig {
            label: v.label(),
            pool,
            engine,
            task_id,
            gate: v.gate,
            pr: v.pr,
            repo,
            _workflows_dir: workflows_dir,
        };
        (rig, def)
    }

    /// The state a task parked at the gate has: `stages` is the capture
    /// map, which decides what `PR_NUMBER` renders to.
    fn parked_payload(v: Variant, stages: Value) -> Value {
        json!({
            "task": { "title": "Add a small feature", "input": "x" },
            "stages": stages,
            "arrival": v.arrival(),
        })
    }

    /// Creates the task and enters its gate through the engine.
    async fn enter(v: Variant, stages: Value, claude: &str) -> ParkedRig {
        let (rig, def) = Self::build(v, claude).await;
        let mut payload = Self::parked_payload(v, stages);
        set_poll_window(&mut payload, &def, v.gate, rig.engine.now()).unwrap();
        workflow_state::create(
            &rig.pool,
            &rig.task_id,
            v.gate,
            "human_gate",
            payload.clone(),
        )
        .await
        .unwrap();
        rig.engine
            .enter_stage(
                &rig.task_id,
                &def,
                v.gate,
                None,
                payload["arrival"]["outcome"].as_str(),
                &payload,
                None,
            )
            .await
            .unwrap();
        rig
    }

    /// A gate row as the previous daemon left it: no poll window, no stage
    /// kind, no runner. The startup sweep has to bring its watcher back.
    async fn legacy_row(v: Variant, stages: Value) -> ParkedRig {
        let (rig, _def) = Self::build(v, "unused").await;
        let payload = Self::parked_payload(v, stages);
        workflow_state::create(&rig.pool, &rig.task_id, v.gate, "human_gate", payload)
            .await
            .unwrap();
        sqlx::query("UPDATE workflow_state SET stage_kind = NULL WHERE task_id = ?")
            .bind(&rig.task_id)
            .execute(&rig.pool)
            .await
            .unwrap();
        rig
    }

    /// A task whose open_pr captured the PR, as the real stage does.
    async fn with_pr(v: Variant) -> ParkedRig {
        Self::with_pr_and_claude(v, "unused").await
    }

    async fn with_pr_and_claude(v: Variant, claude: &str) -> ParkedRig {
        let stages = json!({
            "open_pr": { "number": v.pr, "url": format!("https://example.test/pr/{}", v.pr) }
        });
        Self::enter(v, stages, claude).await
    }

    /// Waits until the stub has answered `state` to `reads` reads of this
    /// PR. The second of two is only made after the first attempt was
    /// handled in full, so a gate that matched it would have left by then.
    async fn wait_served(&self, stub: &GhStub, state: &str, reads: usize) {
        let what = format!("{}: {reads} reads answering {state}", self.label);
        crate::test_support::wait_until(&what, || async {
            let served = stub.served(self.pr);
            let n = served.iter().filter(|s| s.as_str() == state).count();
            if n >= reads {
                Ok(())
            } else {
                Err(format!("served {served:?}"))
            }
        })
        .await
    }

    /// Waits for a persisted attempt event that satisfies `ok`.
    async fn wait_attempt(&self, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
        let what = format!("{}: {what}", self.label);
        crate::test_support::wait_until(&what, || async {
            let events = poll_events(&self.pool, &self.task_id).await;
            events
                .iter()
                .find(|e| ok(e))
                .cloned()
                .ok_or(format!("{} attempt events", events.len()))
        })
        .await
    }

    async fn assert_still_parked(&self, stub: &GhStub) {
        let state = state_of(&self.pool, &self.task_id).await;
        assert_eq!(state.current_stage, self.gate, "{}", self.label);
        let task = tasks::get(&self.pool, &self.task_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.status, "open", "{}", self.label);
        assert!(
            self.engine.has_detached_runner(&self.task_id),
            "{}: the merge watcher is still running",
            self.label
        );
        assert_eq!(stub.unexpected(), "", "{}", self.label);
        self.assert_no_paid_lap().await;
    }

    async fn assert_no_paid_lap(&self) {
        let sessions = sessions::list_for_task(&self.pool, &self.task_id)
            .await
            .unwrap();
        assert!(sessions.is_empty(), "{}: {sessions:?}", self.label);
        let trail = stage_trail(&self.pool, &self.task_id).await;
        assert!(
            trail.iter().all(|(stage, _)| stage == self.gate),
            "{}: {trail:?}",
            self.label
        );
    }

    /// The task closed because its PR merged: done, closed (not
    /// cancelled), arrived from the gate with outcome `merged`, and no
    /// coder, reviewer or PR step ran on the way.
    async fn wait_closed_by_merge(&self, stub: &GhStub) {
        wait_until_stage(&self.pool, &self.task_id, "done").await;
        wait_until_task_status(&self.pool, &self.task_id, "closed").await;
        let state = state_of(&self.pool, &self.task_id).await;
        assert_eq!(
            state.payload["arrival"],
            json!({ "from": self.gate, "outcome": "merged" }),
            "{}",
            self.label
        );
        // Re-entering the gate (a resume does) is not another stage.
        let mut trail: Vec<String> = stage_trail(&self.pool, &self.task_id)
            .await
            .into_iter()
            .map(|(stage, _)| stage)
            .collect();
        trail.dedup();
        assert_eq!(trail, [self.gate, "done"], "{}", self.label);
        let sessions = sessions::list_for_task(&self.pool, &self.task_id)
            .await
            .unwrap();
        assert!(sessions.is_empty(), "{}: {sessions:?}", self.label);
        assert_eq!(stub.unexpected(), "", "{}", self.label);
        assert!(
            !stub.all_calls().contains("create"),
            "{}: a PR was created",
            self.label
        );
        // The matching runner ends itself once the transition is done.
        crate::test_support::wait_until(&format!("{}: no runner left", self.label), || async {
            if self.engine.has_detached_runner(&self.task_id) {
                Err("a runner is still registered".to_string())
            } else {
                Ok(())
            }
        })
        .await;
    }

    async fn finish(&self) {
        self.engine.abort_detached_runners(&self.task_id).await;
        let _ = worktree::remove(&self.repo, "demo", &self.task_id).await;
    }
}

/// Runs `body` for all six variants at once.
macro_rules! for_all_variants {
    ($body:expr) => {{
        let body = $body;
        tokio::join!(
            body(VARIANTS[0]),
            body(VARIANTS[1]),
            body(VARIANTS[2]),
            body(VARIANTS[3]),
            body(VARIANTS[4]),
            body(VARIANTS[5]),
        );
    }};
}

#[tokio::test]
async fn parked_gates_close_after_pr_merge() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in VARIANTS {
        stub.set_record(v.pr, OPEN_WITH_REVIEW_DATA);
    }
    let stub = &stub;
    for_all_variants!(|v: Variant| async move {
        let rig = ParkedRig::with_pr(v).await;
        // A completed attempt that left the task where it was.
        rig.wait_served(stub, "OPEN", 2).await;
        rig.assert_still_parked(stub).await;

        stub.set_record(v.pr, MERGED);
        rig.wait_closed_by_merge(stub).await;
        let task = tasks::get(&rig.pool, &rig.task_id).await.unwrap().unwrap();
        assert_ne!(task.status, "cancelled", "{}", rig.label);
        rig.finish().await;
    });
}

#[tokio::test]
async fn parked_gates_close_when_the_pr_is_already_merged() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in VARIANTS {
        stub.set_record(v.pr, MERGED);
    }
    let stub = &stub;
    for_all_variants!(|v: Variant| async move {
        let rig = ParkedRig::with_pr(v).await;
        // No attempt wait first: this fails at the closure itself when the
        // gate under test has no merge watcher.
        rig.wait_closed_by_merge(stub).await;
        rig.finish().await;
    });
}

#[tokio::test]
async fn parked_gates_hold_on_nonmerge_and_gh_error() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in VARIANTS {
        stub.set_record(v.pr, OPEN_WITH_REVIEW_DATA);
    }
    let stub = &stub;
    for_all_variants!(|v: Variant| async move {
        let rig = ParkedRig::with_pr(v).await;

        // OPEN and conflicting, with an approval and a change request on it.
        rig.wait_served(stub, "OPEN", 2).await;
        rig.assert_still_parked(stub).await;

        // CLOSED without a merge is not a merge.
        stub.set_record(v.pr, CLOSED_UNMERGED);
        rig.wait_served(stub, "CLOSED", 2).await;
        rig.assert_still_parked(stub).await;

        // A failed read, even one that printed MERGED first, is recorded
        // as a failed attempt and moves nothing.
        stub.set_failing(v.pr, true);
        let failed = rig
            .wait_attempt("the failed read to be recorded", |e| {
                e["exit_code"] == json!(17)
            })
            .await;
        assert!(
            failed["stderr_tail"]
                .as_str()
                .unwrap()
                .contains(FAILED_READ_NOTE),
            "{}: {failed}",
            rig.label
        );
        assert_eq!(failed["stdout_tail"], json!(""), "{}", rig.label);
        rig.wait_served(stub, "failed", 2).await;
        rig.assert_still_parked(stub).await;

        // The next read that succeeds and says MERGED closes it.
        stub.set_failing(v.pr, false);
        stub.set_record(v.pr, MERGED);
        rig.wait_closed_by_merge(stub).await;

        rig.finish().await;
    });
    // Every call any variant made was the one state read of its own PR.
    let calls = stub.all_calls();
    let mut rest = calls.as_str();
    let probes: Vec<String> = VARIANTS.iter().map(|v| merge_probe_call(v.pr)).collect();
    while !rest.is_empty() {
        let probe = probes
            .iter()
            .find(|p| rest.starts_with(p.as_str()))
            .unwrap_or_else(|| panic!("an unexpected call in: {rest}"));
        rest = &rest[probe.len()..];
    }
}

/// Every way the PR capture can fail to give a number, as the task's
/// capture map. Each renders `PR_NUMBER` empty through the existing
/// template rules; none is validated by anything new.
fn no_pr_shapes() -> Vec<(&'static str, Value)> {
    vec![
        ("no capture at all", json!({})),
        ("a null capture", json!({ "open_pr": null })),
        ("a text capture", json!({ "open_pr": "no pull request" })),
        ("an array capture", json!({ "open_pr": [1, 2] })),
        (
            "no number field",
            json!({ "open_pr": { "url": "https://example.test/pr/9" } }),
        ),
        ("a null number", json!({ "open_pr": { "number": null } })),
        ("an empty number", json!({ "open_pr": { "number": "" } })),
        (
            "an object number",
            json!({ "open_pr": { "number": { "n": 9 } } }),
        ),
        ("an array number", json!({ "open_pr": { "number": [9] } })),
    ]
}

#[tokio::test]
async fn parked_gates_without_pr_keep_waiting() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    let stub = &stub;
    for (shape, stages) in no_pr_shapes() {
        // What the gate's `env:` really renders to, before anything runs.
        let payload = ParkedRig::parked_payload(VARIANTS[0], stages.clone());
        let (rendered, _) =
            crate::template::render("{{ stages.open_pr.number }}", &payload).unwrap();
        assert_eq!(rendered, "", "{shape}");

        let both = |v: Variant| {
            let stages = stages.clone();
            async move {
                let rig = ParkedRig::enter(v, stages, "unused").await;
                // An attempt that ran and found nothing.
                let attempt = rig
                    .wait_attempt("a completed quiet attempt", is_real_attempt)
                    .await;
                assert_eq!(attempt["exit_code"], json!(0), "{} {shape}", rig.label);
                assert_eq!(attempt["stdout_tail"], json!(""), "{} {shape}", rig.label);
                rig.assert_still_parked(stub).await;
                rig.finish().await;
            }
        };
        tokio::join!(both(VARIANTS[0]), both(VARIANTS[1]));
        assert_eq!(stub.all_calls(), "", "{shape}: gh must not be called");
    }
}

#[tokio::test]
async fn a_note_at_a_parked_gate_without_a_pr_still_goes_to_revising() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    let note = "Please keep the retry logic.\n\nAnd leave the cache alone: it is slow on purpose.";
    for v in [VARIANTS[0], VARIANTS[1]] {
        let rig = ParkedRig::enter(v, json!({}), &stub.claude()).await;
        rig.wait_attempt("a quiet attempt", is_real_attempt).await;
        rig.engine
            .send_message_or_resume(&rig.task_id, note)
            .await
            .unwrap();
        let state = state_of(&rig.pool, &rig.task_id).await;
        assert_eq!(state.current_stage, "revising", "{}", rig.label);
        assert_eq!(
            state.payload["stages"][rig.gate],
            json!(note),
            "{}",
            rig.label
        );
        assert_eq!(
            state.payload["arrival"],
            json!({ "from": rig.gate, "outcome": "resumed" })
        );
        let messages = human_messages(&rig.pool, &rig.task_id).await;
        assert_eq!(messages.len(), 1, "{}", rig.label);
        assert_eq!(messages[0]["text"], json!(note));
        assert!(
            !rig.engine.has_detached_runner(&rig.task_id),
            "{}: the merge watcher is stopped",
            rig.label
        );
        // The coder's prompt shows the full note.
        let prompt = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/prompts/coder-revise.md"),
        )
        .unwrap();
        let (rendered, _) = crate::template::render(&prompt, &state.payload).unwrap();
        assert!(rendered.contains(note), "{}: {rendered}", rig.label);
        assert_eq!(stub.all_calls(), "");
        rig.engine.cancel_task(&rig.task_id, false).await.unwrap();
    }
}

#[tokio::test]
async fn a_note_at_a_parked_gate_with_a_pr_stops_the_merge_watcher() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    let note = "The reviewer asked for X.\nPlease also rename Y.";
    for v in [VARIANTS[0], VARIANTS[1]] {
        stub.set_record(v.pr, OPEN_WITH_REVIEW_DATA);
        let rig = ParkedRig::with_pr_and_claude(v, &stub.claude()).await;
        rig.wait_served(&stub, "OPEN", 1).await;
        rig.engine
            .send_message_or_resume(&rig.task_id, note)
            .await
            .unwrap();
        let state = state_of(&rig.pool, &rig.task_id).await;
        assert_eq!(state.current_stage, "revising", "{}", rig.label);
        assert_eq!(
            state.payload["stages"][rig.gate],
            json!(note),
            "{}",
            rig.label
        );
        assert!(!rig.engine.has_detached_runner(&rig.task_id));
        // Nothing reads the PR any more: a merge now does not move the task.
        let reads = stub.served(v.pr).len();
        stub.set_record(v.pr, MERGED);
        tokio::time::sleep(StdDuration::from_secs(3)).await;
        assert_eq!(
            stub.served(v.pr).len(),
            reads,
            "{}: the watcher ran on",
            rig.label
        );
        assert_ne!(
            state_of(&rig.pool, &rig.task_id).await.current_stage,
            "done",
            "{}",
            rig.label
        );
        rig.engine.cancel_task(&rig.task_id, false).await.unwrap();
    }
}

#[tokio::test]
async fn a_marker_only_reply_at_a_parked_gate_changes_nothing() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in [VARIANTS[0], VARIANTS[1]] {
        stub.set_record(v.pr, OPEN_WITH_REVIEW_DATA);
        let rig = ParkedRig::with_pr(v).await;
        rig.wait_served(&stub, "OPEN", 1).await;
        let before = state_of(&rig.pool, &rig.task_id).await;
        // The window and capture slots cannot change under a refused reply.
        for text in ["/approve", "/request-changes\n\n"] {
            let err = rig
                .engine
                .send_message_or_resume(&rig.task_id, text)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, SendMessageOrResumeError::ReplyIsOnlyMarkers { stage, .. } if stage == rig.gate),
                "{}: {err:?}",
                rig.label
            );
            assert!(human_messages(&rig.pool, &rig.task_id).await.is_empty());
            assert_eq!(state_of(&rig.pool, &rig.task_id).await, before);
            assert!(rig.engine.has_detached_runner(&rig.task_id));
        }
        rig.finish().await;
    }
}

#[tokio::test]
async fn a_merge_while_a_gate_is_paused_closes_it_only_after_resume() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in [VARIANTS[0], VARIANTS[1]] {
        stub.set_record(v.pr, OPEN_WITH_REVIEW_DATA);
        let rig = ParkedRig::with_pr(v).await;
        rig.wait_served(&stub, "OPEN", 1).await;

        rig.engine.pause_task(&rig.task_id, false).await.unwrap();
        let task = tasks::get(&rig.pool, &rig.task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "paused", "{}", rig.label);
        assert!(!rig.engine.has_detached_runner(&rig.task_id));
        // The PR merges while the task is paused; nothing may notice.
        let reads = stub.served(v.pr).len();
        stub.set_record(v.pr, MERGED);
        tokio::time::sleep(StdDuration::from_secs(3)).await;
        assert_eq!(
            stub.served(v.pr).len(),
            reads,
            "{}: read while paused",
            rig.label
        );
        let task = tasks::get(&rig.pool, &rig.task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "paused", "{}", rig.label);
        assert_eq!(
            state_of(&rig.pool, &rig.task_id).await.current_stage,
            rig.gate
        );

        // Resuming starts the watcher again, and the merge closes the task.
        rig.engine.resume_task(&rig.task_id).await.unwrap();
        rig.wait_closed_by_merge(&stub).await;
        rig.finish().await;
    }
}

#[tokio::test]
async fn a_merge_after_a_gate_was_cancelled_leaves_it_cancelled() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in [VARIANTS[0], VARIANTS[1]] {
        stub.set_record(v.pr, OPEN_WITH_REVIEW_DATA);
        let rig = ParkedRig::with_pr(v).await;
        rig.wait_served(&stub, "OPEN", 1).await;

        rig.engine.cancel_task(&rig.task_id, false).await.unwrap();
        assert!(!rig.engine.has_detached_runner(&rig.task_id));
        let reads = stub.served(v.pr).len();
        stub.set_record(v.pr, MERGED);
        tokio::time::sleep(StdDuration::from_secs(3)).await;
        assert_eq!(
            stub.served(v.pr).len(),
            reads,
            "{}: read after cancel",
            rig.label
        );
        let task = tasks::get(&rig.pool, &rig.task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "cancelled", "{}", rig.label);
        assert_eq!(
            state_of(&rig.pool, &rig.task_id).await.current_stage,
            rig.gate
        );
    }
}

/// A task the previous daemon left at a parked gate: no poll window, no
/// stage kind, no runner. The startup sweep restores its kind and starts
/// the merge watcher once; with no PR it keeps waiting on a human, with a
/// merged one it closes.
#[tokio::test]
async fn an_escalated_task_keeps_waiting_on_a_human_after_the_upgrade() {
    let stub = GhStub::new();
    let _path = PathPrefixGuard::new(&stub.root);
    for v in [VARIANTS[0], VARIANTS[1]] {
        // No PR: parked, watched, waiting on a human, gh never called.
        let calls_before = stub.all_calls();
        let rig = ParkedRig::legacy_row(v, json!({})).await;
        rig.engine.park_interrupted_turns().await.unwrap();
        let report = rig.engine.resume_interrupted_polls().await.unwrap();
        assert_eq!(report.resumed, 1, "{}: {report:?}", rig.label);
        // A second sweep must not start a second watcher.
        let again = rig.engine.resume_interrupted_polls().await.unwrap();
        assert_eq!(again.resumed, 0, "{}: {again:?}", rig.label);
        assert_eq!(runner_slots(&rig.engine, &rig.task_id), 1, "{}", rig.label);
        rig.wait_attempt("a quiet attempt", is_real_attempt).await;
        let task = tasks::get(&rig.pool, &rig.task_id).await.unwrap().unwrap();
        assert_eq!(task.status, "open", "{}", rig.label);
        let state = state_of(&rig.pool, &rig.task_id).await;
        assert_eq!(state.current_stage, rig.gate);
        assert_eq!(state.stage_kind.as_deref(), Some("human_gate"));
        let summaries = tasks::list_summaries(&rig.pool, None, &[], tasks::SummaryOrder::Id, None)
            .await
            .unwrap();
        let summary = summaries.iter().find(|t| t.task.id == rig.task_id).unwrap();
        assert!(summary.waiting_on_human, "{}", rig.label);
        assert_eq!(stub.all_calls(), calls_before, "{}", rig.label);
        rig.finish().await;

        // A captured PR that is already merged: the recovered watcher closes it.
        let merged_v = Variant {
            pr: v.pr + 100,
            ..v
        };
        stub.set_record(merged_v.pr, MERGED);
        let stages = json!({ "open_pr": { "number": merged_v.pr } });
        let rig = ParkedRig::legacy_row(merged_v, stages).await;
        rig.engine.park_interrupted_turns().await.unwrap();
        let report = rig.engine.resume_interrupted_polls().await.unwrap();
        assert_eq!(report.resumed, 1, "{}: {report:?}", rig.label);
        rig.wait_closed_by_merge(&stub).await;
        rig.finish().await;
    }
}
