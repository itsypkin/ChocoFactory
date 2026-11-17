//! Production-model probes of the #369 prompts, and the fixture they run on.
//!
//! The history a probe reads is a FIXTURE: production-shaped stage results
//! (kinds, outcomes, destinations, captures and `previous_result_id` links as
//! the engine writes them) inserted into a private in-process daemon's
//! database, so the real `get_context` lookup serves them to the real CLI. It
//! is not a recording of a live multi-stage run. The repositories are real
//! disposable git repositories with distinct commits.
//!
//! Every fixture is built by one function, shared by the free test that
//! checks it through the real tool (`the_probe_fixtures_are_served_by_the_real_lookup`,
//! default suite, no model) and the paid probes below, so the two cannot
//! drift. The paid probes are `#[ignore]`d and run only when named with
//! `CHOCO_REAL_PROBE=<name>`:
//!
//! ```text
//! CHOCO_REAL_PROBE=mixed_rereview cargo test -p chocofactoryd --lib \
//!   context_probe_tests::probe_mixed_rereview -- --ignored --exact --nocapture
//! ```
//!
//! Each paid probe is one real turn: Opus 5.5 (`claude-opus-5-5`, high) for
//! the reviewer, Sonnet 5.5 (`claude-sonnet-5-5`, high) for the coder, the
//! operator's normal login, the shipped system and turn prompts copied from
//! `workflows/prompts` unchanged. They assert on recorded tool calls, the
//! reported outcome and real git facts, never on the prompt's own words. They
//! show what one sample did; the deterministic prompt tests show what the
//! prompts say.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chocofactory_core::context::StageResultPayload;
use chocofactory_core::context_tool::ContextBinding;
use chocofactory_core::models::EventType;
use serde_json::{Value, json};

use crate::api::tests::TestServer;
use crate::context_tool_tests::{choco_binary, via_choco};
use crate::db::{events, tasks, workflow_state};

// ---------------------------------------------------------------------------
// The disposable repositories
// ---------------------------------------------------------------------------

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.email=probe@example.test",
            "-c",
            "user.name=probe",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// One commit: the files it writes (path, content) and its message. Returns
/// the full SHA.
fn commit(repo: &Path, message: &str, files: &[(&str, &str)]) -> String {
    for (path, content) in files {
        let full = repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        if path.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
}

// ---------------------------------------------------------------------------
// Seeding stage results
// ---------------------------------------------------------------------------

/// One recorded departure, as the engine would write it.
#[derive(Clone)]
pub(crate) struct Seed {
    stage: &'static str,
    kind: &'static str,
    outcome: &'static str,
    destination: &'static str,
    capture: Option<Value>,
}

/// An agent turn's departure: its capture is `{outcome, summary}`.
fn agent(stage: &'static str, outcome: &'static str, to: &'static str, summary: String) -> Seed {
    Seed {
        stage,
        kind: "agent_turn",
        outcome,
        destination: to,
        capture: Some(json!({"outcome": outcome, "summary": summary})),
    }
}

/// A human gate or a poll: its capture is text.
fn text(
    stage: &'static str,
    kind: &'static str,
    outcome: &'static str,
    to: &'static str,
    capture: String,
) -> Seed {
    Seed {
        stage,
        kind,
        outcome,
        destination: to,
        capture: Some(Value::String(capture)),
    }
}

/// A shell stage's departure with a JSON capture.
fn shell(stage: &'static str, outcome: &'static str, to: &'static str, capture: Value) -> Seed {
    Seed {
        stage,
        kind: "shell",
        outcome,
        destination: to,
        capture: Some(capture),
    }
}

/// Inserts `seeds`, oldest first, linking each to the one before, and
/// returns their result IDs.
async fn record(server: &TestServer, task_id: &str, seeds: &[Seed]) -> Vec<String> {
    let mut previous: Option<String> = None;
    let mut ids = Vec::new();
    for seed in seeds {
        let payload = StageResultPayload::new(
            seed.stage,
            seed.kind,
            seed.outcome,
            seed.destination,
            seed.capture.clone(),
            previous.clone(),
        )
        .to_value();
        let event =
            events::append_for_task(server.pool(), task_id, EventType::StageResult, payload)
                .await
                .unwrap();
        previous = Some(event.id.clone());
        ids.push(event.id);
    }
    ids
}

// ---------------------------------------------------------------------------
// The scenarios
// ---------------------------------------------------------------------------

/// Which prompt pair the probed turn is given.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Turn {
    /// `reviewer-system.md` + `reviewer-turn.md`, from `revising`.
    Reviewer,
    /// `reviewer-system.md` + `reviewer-turn-planned.md`, from `revising`.
    PlannedReviewer,
    /// `coder-system.md` + `coder-revise.md`, arriving from `from`.
    Coder {
        from: &'static str,
        outcome: &'static str,
    },
}

/// Everything a probe needs: the repository, the seeded history, the turn.
pub(crate) struct Scenario {
    pub repo: PathBuf,
    pub title: &'static str,
    /// What the task was started with (`{"source":"task"}` returns it).
    pub input: String,
    pub history: Vec<Seed>,
    pub turn: Turn,
    /// Full SHAs of the commits, base first.
    pub commits: Vec<String>,
    /// Result IDs to delete after seeding, by index into `history`: the
    /// retention job pruned them.
    pub prune: Vec<usize>,
}

const LONG_PAD: &str = "This is realistic hand-off prose, not a summary: the person who wrote it \
was reading the diff on the pull request, had the ticket open in another window, and wrote down \
everything they wanted so that nobody would have to guess or ask. ";

fn long(n: usize) -> String {
    LONG_PAD.repeat(n)
}

// ---- 1: mixed reviewer-started re-review (the ordinary workflow) ----------

pub(crate) fn mixed_rereview(repo: &Path) -> Scenario {
    init_repo(repo);
    let sync_base = "#!/usr/bin/env bash\n# Usage: sync.sh SRC DEST [-v]\nset -euo pipefail\n. \"$(dirname \"$0\")/../lib/copy.sh\"\nsrc=$1; dest=$2; verbose=0\n[ \"${3:-}\" = \"-v\" ] && verbose=1\nmkdir -p \"$dest\"\nfor f in \"$src\"/*; do\n  copy_file \"$f\" \"$dest\" \"$verbose\"\ndone\n";
    let copy_sh = "# Copies one file into DEST. Housekeeping first: stale backups are cleared.\ncopy_file() {\n  local f=$1 dest=$2 verbose=$3 name\n  name=$(basename \"$f\")\n  find \"$dest\" -maxdepth 1 -name '*.bak' -delete\n  cp \"$f\" \"$dest/$name\"\n  [ \"$verbose\" = 1 ] && echo \"copied $name\"\n  return 0\n}\n";
    let readme0 =
        "# filesync\n\nCopies every file of SRC into DEST.\n\n    bin/sync.sh SRC DEST [-v]\n";
    let base = commit(
        repo,
        "Add the sync tool",
        &[
            ("bin/sync.sh", sync_base),
            ("lib/copy.sh", copy_sh),
            ("README.md", readme0),
        ],
    );
    // A: the coder's first attempt. Dry-run is parsed, but DEST is still created.
    let sync_a = "#!/usr/bin/env bash\n# Usage: sync.sh SRC DEST [-v] [--dry-run]\nset -euo pipefail\n. \"$(dirname \"$0\")/../lib/copy.sh\"\nsrc=$1; dest=$2; verbose=0; dry=0\nfor arg in \"${@:3}\"; do\n  case $arg in\n    --dry-run) dry=1 ;;\n    -v) verbose=1 ;;\n  esac\ndone\nmkdir -p \"$dest\"\nfor f in \"$src\"/*; do\n  if [ \"$dry\" = 1 ]; then echo \"would copy $(basename \"$f\")\"; [ \"$verbose\" = 1 ] && echo \"  from $f\"; continue; fi\n  copy_file \"$f\" \"$dest\" \"$verbose\"\ndone\n";
    let test_a = "#!/usr/bin/env bash\nset -euo pipefail\ncd \"$(dirname \"$0\")/..\"\nt=$(mktemp -d); mkdir \"$t/src\"; echo hi > \"$t/src/a.txt\"\nout=$(bin/sync.sh \"$t/src\" \"$t/dest\" --dry-run)\n[ \"$out\" = \"would copy a.txt\" ] || { echo FAIL output; exit 1; }\n[ ! -e \"$t/dest/a.txt\" ] || { echo FAIL wrote; exit 1; }\necho ok\n";
    let a = commit(
        repo,
        "Add --dry-run to sync.sh",
        &[("bin/sync.sh", sync_a), ("tests/test_dry_run.sh", test_a)],
    );
    // C1 documents the flag: resolves the README blocker.
    let c1 = commit(
        repo,
        "Document --dry-run in the README",
        &[(
            "README.md",
            "# filesync\n\nCopies every file of SRC into DEST.\n\n    bin/sync.sh SRC DEST [-v] [--dry-run]\n\n`--dry-run` prints `would copy NAME` for each file and writes nothing; with `-v` it also prints `  from PATH` under each line.\n",
        )],
    );
    // C2 refactors the parsing: `break` after --dry-run drops a later -v, so
    // `sync.sh SRC DEST --dry-run -v` no longer prints the `  from PATH` lines.
    let sync_c2 = "#!/usr/bin/env bash\n# Usage: sync.sh SRC DEST [-v] [--dry-run]\nset -euo pipefail\n. \"$(dirname \"$0\")/../lib/copy.sh\"\nsrc=$1; dest=$2; verbose=0; dry=0\nfor arg in \"${@:3}\"; do\n  case $arg in\n    --dry-run) dry=1; break ;;\n    -v) verbose=1 ;;\n    *) echo \"unknown flag $arg\" >&2; exit 2 ;;\n  esac\ndone\nmkdir -p \"$dest\"\nfor f in \"$src\"/*; do\n  if [ \"$dry\" = 1 ]; then echo \"would copy $(basename \"$f\")\"; [ \"$verbose\" = 1 ] && echo \"  from $f\"; continue; fi\n  copy_file \"$f\" \"$dest\" \"$verbose\"\ndone\n";
    let c2 = commit(
        repo,
        "Tidy the argument parsing and reject unknown flags",
        &[("bin/sync.sh", sync_c2)],
    );

    let input = format!(
        "Add a `--dry-run` flag to bin/sync.sh.\n\nWith it the script prints `would copy NAME` for each file of SRC and changes nothing on \
disk: no directory is created and no file is written. `-v` must keep working, in either order \
with `--dry-run`: in a dry run `-v` also prints `  from PATH` under each line. The tool must never delete a file in DEST that SRC does not replace: that \
promise is the reason people trust it with their backups. Add tests/test_dry_run.sh and mention \
the flag in README.md. {}",
        long(2)
    );
    let r1 = format!(
        "Prior findings: none (first review).\n\nReviewed: {a}\n\nPredictions: dry-run may still touch DEST; the README may not mention the flag; the test may not check the disk.\n\nBranches → tests: bin/sync.sh dry-run branch is covered by tests/test_dry_run.sh for output only.\n\nStates: none.\n\nMessages: none.\n\nSide effects: bin/sync.sh:12 runs `mkdir -p \"$dest\"` before the dry-run check, so `--dry-run` creates DEST.\n\nResources and work: none.\n\nOld behaviour: unchanged without the flag.\n\nFindings:\n1. BLOCKING bin/sync.sh:12: `--dry-run` still creates DEST (`mkdir -p` runs first). The task says nothing is created. Input: `sync.sh src /tmp/new --dry-run`; /tmp/new now exists. Fix: skip `mkdir` when `dry` is 1, and make tests/test_dry_run.sh assert DEST does not exist.\n2. BLOCKING README.md: the new flag is not mentioned, which the task requires.\n\nDismissed: none."
    );
    let coder_report = format!(
        "Branch: --dry-run for bin/sync.sh.\n\nSent back for: (1) `--dry-run` creates DEST; (2) README does not mention the flag.\n\n(1) DEST creation: changed, in {} (Tidy the argument parsing...). The arguments are parsed once up front now. No separate commit.\n(2) README: changed, commit {} (Document --dry-run in the README).\n\nVerified: ran tests/test_dry_run.sh, prints ok.\nNot done: none.",
        &c2[..8],
        &c1[..8]
    );
    let history = vec![
        agent(
            "coding",
            "done",
            "internal_review",
            format!(
                "Implemented --dry-run in bin/sync.sh with a test, commit {}. Verified: tests/test_dry_run.sh prints ok. Not done: README.",
                &a[..8]
            ),
        ),
        agent("internal_review", "changes_requested", "revising", r1),
        agent("revising", "done", "internal_review", coder_report),
    ];
    Scenario {
        repo: repo.to_path_buf(),
        title: "Add --dry-run to sync.sh",
        input,
        history,
        turn: Turn::Reviewer,
        commits: vec![base, a, c1, c2],
        prune: vec![],
    }
}

// ---- 2: a human-started round carried through another internal correction --

const PLANNER_SPEC: &str = "Checks: the repository has lib/retry.sh with an unbounded backoff; README.md documents RETRY_MAX.\n\nDecisions: cap the delay at 30 seconds; keep jitter configurable through RETRY_JITTER; a 12-hour soak run of the retry loop must be completed and reported before approval.\n\nQuestions: none.\n\nSpec:\n1. lib/retry.sh must cap the backoff delay at 30 seconds.\n2. Jitter stays configurable through RETRY_JITTER.\n3. A 12-hour soak run of the retry loop is required as proof before the change is approved.\n4. Out of scope: the HTTP client itself.";

/// Where the human's hand-off sits in `human_scoped_round`'s history.
pub(crate) const HUMAN_HANDOFF: usize = 8;

pub(crate) fn human_scoped_round(repo: &Path) -> Scenario {
    init_repo(repo);
    let retry0 = "# Retry helper. Delay doubles each attempt.\nretry_delay() {\n  local attempt=$1\n  echo $((2 ** attempt))\n}\n";
    let base = commit(
        repo,
        "Add the retry helper",
        &[
            ("lib/retry.sh", retry0),
            (
                "README.md",
                "# retrykit\n\nSet RETRY_MAX to the number of attempts. Set RETRY_JITTER for jitter.\n",
            ),
        ],
    );
    let retry1 = "# Retry helper. Delay doubles each attempt, capped at 30 seconds.\nretry_delay() {\n  local attempt=$1 delay\n  delay=$((2 ** attempt))\n  [ \"$delay\" -gt 30 ] && delay=30\n  echo \"$delay\"\n}\n";
    let c0 = commit(repo, "Cap the retry delay", &[("lib/retry.sh", retry1)]);
    // H1 (human item 1): the cap also guards a negative attempt.
    let retry2 = "# Retry helper. Delay doubles each attempt, capped at 30 seconds.\nretry_delay() {\n  local attempt=$1 delay\n  [ \"$attempt\" -lt 0 ] && attempt=0\n  delay=$((2 ** attempt))\n  [ \"$delay\" -gt 30 ] && delay=30\n  echo \"$delay\"\n}\n";
    let h1 = commit(
        repo,
        "Clamp a negative attempt count",
        &[("lib/retry.sh", retry2)],
    );
    // H2 (human item 2): the support script. Defective: it recomputes the
    // delay itself, with no cap, and always exits 0.
    let verify = "#!/usr/bin/env bash\n# Prints the delay for attempts 1..8 so the cap can be eyeballed.\nfor n in $(seq 1 8); do\n  echo \"attempt $n: $((2 ** n))s\"\ndone\nexit 0\n";
    let h2 = commit(
        repo,
        "Add scripts/verify-backoff.sh",
        &[("scripts/verify-backoff.sh", verify)],
    );
    // K1: another internal correction (a test for the cap).
    let test = "#!/usr/bin/env bash\nset -euo pipefail\n. \"$(dirname \"$0\")/../lib/retry.sh\"\n[ \"$(retry_delay 9)\" = 30 ] || { echo FAIL; exit 1; }\necho ok\n";
    let k1 = commit(
        repo,
        "Test the 30 second cap",
        &[("tests/test_retry.sh", test)],
    );
    // K2: the CI lap. Quote a variable.
    let retry3 = retry2.replace("echo \"$delay\"", "printf '%s\\n' \"$delay\"");
    let k2 = commit(
        repo,
        "Use printf in retry_delay (shellcheck)",
        &[("lib/retry.sh", &retry3)],
    );

    let human = format!(
        "Thanks for the work so far. I read the whole diff on the PR this morning and I want exactly three \
things, nothing else.\n\n1. (item 1) In lib/retry.sh, make retry_delay safe for a negative attempt \
count: treat it as 0.\n2. (item 2) Add scripts/verify-backoff.sh that runs the real retry_delay for \
attempts 1 to 8 and prints each delay, so I can check the 30 second cap by eye. It must exercise \
lib/retry.sh, not recompute the numbers.\n3. (item 3) Rename the environment variable RETRY_MAX to \
HTTP_RETRY_MAX everywhere it appears (lib/retry.sh and README.md).\n\nAccepted costs: I did NOT run \
the 12-hour soak test and I accept that. Nobody should ask for it again, not the reviewer and not \
you. The spec's wording about jitter is also fine as it is; leave RETRY_JITTER alone.\n\n{}Change nothing else.",
        long(3)
    );
    let r1 = format!(
        "Prior findings: none.\n\nReviewed: {c0}\n\nFindings:\n1. BLOCKING: the 12-hour soak run required by the spec was not made.\n2. BLOCKING lib/retry.sh: RETRY_JITTER is not read anywhere.\n\nDismissed: none."
    );
    let r2 = format!(
        "Prior findings: 1 not made (the soak run, named for the human), 2 resolved.\n\nReviewed: {c0}\n\nFindings: none.\n\nDismissed: none.\n\nGate: no test script yet; nothing to run."
    );
    let r3 = format!(
        "Prior findings: item 3 not resolved.\n\nReviewed: {h2}\n\nFindings:\n1. BLOCKING: the 12-hour soak run required by the spec is still not made.\n2. BLOCKING: RETRY_JITTER is still not read in lib/retry.sh; the spec requires jitter to be configurable.\n3. BLOCKING scripts/verify-backoff.sh:4: no test for the cap.\n\nDismissed: none."
    );
    let history = vec![
        agent("spec_check", "ready", "coding", PLANNER_SPEC.to_string()),
        agent("coding", "done", "internal_review", format!("Capped the retry delay at 30 seconds, commit {}. Verified by hand. Not done: the 12-hour soak run; jitter.", &c0[..8])),
        agent("internal_review", "changes_requested", "revising", r1),
        agent("revising", "done", "internal_review", "The 12-hour soak run can't be made in this environment; recorded as not made. Jitter is configurable through RETRY_JITTER, documented in the README.".to_string()),
        agent("internal_review", "approved", "describe_pr", r2),
        agent("describe_pr", "done", "open_pr", "Retry delay is now capped at 30 seconds.".to_string()),
        shell("open_pr", "done", "checks_polling", json!({"number": 41, "url": "https://example.test/pull/41"})),
        text("checks_polling", "poll", "green", "awaiting_human_review", "GREEN\n".to_string()),
        text("awaiting_human_review", "human_gate", "changes_requested", "revising", format!("REQUEST_CHANGES\n\n### reviewer (OWNER), 2026-11-15T09:12:00Z\nhttps://example.test/pull/41#issuecomment-1\n\n{human}")),
        agent("revising", "done", "internal_review", format!("Per the human's request.\n(item 1) changed, commit {}: retry_delay treats a negative attempt as 0.\n(item 2) changed, commit {}: scripts/verify-backoff.sh prints the delays.\n(item 3) changed: the variable is renamed to HTTP_RETRY_MAX everywhere, resolved by commit {}.\nNot done: nothing. Soak run not made, accepted by the human.", &h1[..8], &h2[..8], &h2[..8])),
        agent("internal_review", "changes_requested", "revising", r3),
        agent("revising", "done", "internal_review", format!("Internal correction. The soak run and RETRY_JITTER are not changed: the human accepted the missing soak run and said to leave jitter alone. Added a test for the cap, commit {}.", &k1[..8])),
        agent("internal_review", "approved", "describe_pr", format!("Prior findings: all addressed.\n\nReviewed: {k1}\n\nFindings: none.\n\nDismissed: none.")),
        agent("describe_pr", "done", "open_pr", "Retry delay capped at 30 seconds with a verification script.".to_string()),
        shell("open_pr", "done", "checks_polling", json!({"number": 41, "url": "https://example.test/pull/41"})),
        text("checks_polling", "poll", "red", "revising", "RED\nshellcheck: lib/retry.sh line 7: use printf instead of echo (SC2086-style)\n".to_string()),
        agent("revising", "done", "internal_review", format!("CI lap: replaced echo with printf in lib/retry.sh, commit {}.", &k2[..8])),
    ];
    Scenario {
        repo: repo.to_path_buf(),
        title: "Cap the retry delay",
        input: "Cap the backoff in lib/retry.sh.".to_string(),
        history,
        turn: Turn::PlannedReviewer,
        commits: vec![base, c0, h1, h2, k1, k2],
        prune: vec![],
    }
}

// ---- 3: the governing hand-off is gone ---------------------------------------

pub(crate) fn missing_history(repo: &Path) -> Scenario {
    init_repo(repo);
    let base = commit(
        repo,
        "Add the greeter",
        &[
            ("bin/greet.sh", "#!/usr/bin/env bash\necho \"hello $1\"\n"),
            ("README.md", "# greeter\n"),
        ],
    );
    let c1 = commit(
        repo,
        "Greet everyone in the list",
        &[(
            "bin/greet.sh",
            "#!/usr/bin/env bash\nfor n in \"$@\"; do echo \"hello $n\"; done\n",
        )],
    );
    let c2 = commit(
        repo,
        "Print usage without arguments",
        &[(
            "bin/greet.sh",
            "#!/usr/bin/env bash\n[ $# -gt 0 ] || { echo \"usage: greet.sh NAME...\"; exit 2; }\nfor n in \"$@\"; do echo \"hello $n\"; done\n",
        )],
    );
    let human = format!(
        "{}Please make the greeter handle several names, print a usage line without arguments, and change nothing else. I accept that the tests were not run.",
        long(3)
    );
    let history = vec![
        agent(
            "coding",
            "done",
            "internal_review",
            format!("Implemented greet.sh, commit {}.", &base[..8]),
        ),
        agent(
            "internal_review",
            "changes_requested",
            "revising",
            format!(
                "Reviewed: {base}\n\nFindings:\n1. BLOCKING bin/greet.sh:2: greets only the first name."
            ),
        ),
        text(
            "awaiting_human_review",
            "human_gate",
            "changes_requested",
            "revising",
            format!("REQUEST_CHANGES\n\n### reviewer (OWNER), 2026-11-15T09:12:00Z\n\n{human}"),
        ),
        agent(
            "revising",
            "done",
            "internal_review",
            format!(
                "Per the human's request: several names, commit {}; usage line, commit {}. Nothing else changed.",
                &c1[..8],
                &c2[..8]
            ),
        ),
    ];
    Scenario {
        repo: repo.to_path_buf(),
        title: "Greet several names",
        input: "Make bin/greet.sh greet every name it is given.".to_string(),
        history,
        turn: Turn::Reviewer,
        commits: vec![base, c1, c2],
        // The human's hand-off (index 2) has been pruned by the retention job.
        prune: vec![2],
    }
}

// ---- 4: the coder answers every item ----------------------------------------

pub(crate) fn coder_item_mapping(repo: &Path) -> Scenario {
    init_repo(repo);
    let notify = "#!/usr/bin/env bash\n# Usage: notify.sh SUBJECT [BODY]\nset -euo pipefail\n. \"$(dirname \"$0\")/../lib/format.sh\"\nif [ \"${1:-}\" = \"--help\" ]; then echo \"usage: notify.sh SUBJECT [BODY]\"; exit 0; fi\n[ -n \"${1:-}\" ] || { echo \"missing subject\" >&2; exit 1; }\nformat_message \"$1\" \"${2:-}\"\n";
    let format = "# Formats one notification.\nformat_message() {\n  local subject=$1 body=$2 msg\n  msg=$(printf '[notify] %s\\n%s' $subject \"$body\")\n  printf '%s\\n' \"$msg\"\n}\n";
    let base = commit(
        repo,
        "Add notify.sh",
        &[
            ("bin/notify.sh", notify),
            ("lib/format.sh", format),
            (
                "tests/test_notify.sh",
                "#!/usr/bin/env bash\nset -euo pipefail\ncd \"$(dirname \"$0\")/..\"\nout=$(bin/notify.sh --help)\n[ \"$out\" = \"usage: notify.sh SUBJECT [BODY]\" ]\necho ok\n",
            ),
            (
                "README.md",
                "# notify-kit\n\n    bin/notify.sh SUBJECT [BODY]\n",
            ),
        ],
    );
    let human = format!(
        "I'm stepping in because the review loop keeps coming back with the same list. Here is what I \
want, with my answers to the reviewer's items. Use these labels in your report.\n\n(H1) bin/notify.sh \
must exit with status 2, not 1, when SUBJECT is missing.\n(H2) `notify.sh --help` must print the \
usage line.\n(F1) Do the reviewer's first blocking item: lib/format.sh word-splits the subject.\n(F2) \
Do NOT do the reviewer's second blocking item (the 12-hour soak run). I did not run it and I accept \
that; it is an accepted cost, don't ask for it and don't add any proof for it.\n\nThe reviewer's style \
advisory (A1, renaming `msg`) is not a request: leave it.\n\n{}Change nothing else.",
        long(3)
    );
    let review = format!(
        "Prior findings: none.\n\nReviewed: {base}\n\nFindings:\n(F1) BLOCKING lib/format.sh:4: `$subject` is unquoted, so a subject with several spaces is word-split and the spaces collapse. Fix: quote it.\n(F2) BLOCKING: the spec's 12-hour soak run was not made.\n(A1) NONBLOCKING (minor, style): lib/format.sh uses the name `msg`; `message` reads better.\n\nDismissed: none."
    );
    let history = vec![
        agent(
            "coding",
            "done",
            "internal_review",
            format!("Added notify.sh with a help test, commit {}.", &base[..8]),
        ),
        agent(
            "internal_review",
            "changes_requested",
            "revising",
            review.clone(),
        ),
        agent(
            "revising",
            "done",
            "internal_review",
            "No change: waiting for guidance.".to_string(),
        ),
        agent(
            "internal_review",
            "changes_requested",
            "escalate_to_human",
            review,
        ),
        text(
            "escalate_to_human",
            "human_gate",
            "resumed",
            "revising",
            human,
        ),
    ];
    Scenario {
        repo: repo.to_path_buf(),
        title: "Notification formatting",
        input: "Build bin/notify.sh and lib/format.sh: format a notification from a subject and a body.".to_string(),
        history,
        turn: Turn::Coder { from: "escalate_to_human", outcome: "resumed" },
        commits: vec![base],
        prune: vec![],
    }
}

// ---------------------------------------------------------------------------
// Standing the scenario up on a private daemon
// ---------------------------------------------------------------------------

const REVIEW_SECTIONS: &str = "[Reviewed, Predictions, \"Branches → tests\", States, Messages, \"Side effects\", \"Resources and work\", \"Old behaviour\", Findings, Dismissed]";

fn prompts_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workflows/prompts")
}

fn probe_workflow(turn: Turn) -> (String, Vec<&'static str>) {
    let (role, model, system, prompt, stage) = match turn {
        Turn::Reviewer => (
            "reviewer",
            "claude-opus-5-5:high",
            "reviewer-system.md",
            "reviewer-turn.md",
            "internal_review",
        ),
        Turn::PlannedReviewer => (
            "reviewer",
            "claude-opus-5-5:high",
            "reviewer-system.md",
            "reviewer-turn-planned.md",
            "internal_review",
        ),
        Turn::Coder { .. } => (
            "coder",
            "claude-sonnet-5-5:high",
            "coder-system.md",
            "coder-revise.md",
            "revising",
        ),
    };
    let extra = if role == "reviewer" {
        "    read_only: true\n    disallowed_tools: [edit, write, notebook_edit]\n"
    } else {
        ""
    };
    let sections = if role == "reviewer" {
        format!("    report_sections: {REVIEW_SECTIONS}\n")
    } else {
        String::new()
    };
    let on = if role == "reviewer" {
        "{ approved: finished, changes_requested: finished }"
    } else {
        "{ done: hold, blocked: hold }"
    };
    let yaml = format!(
        "name: probe\nworktree: true\nroles:\n  {role}:\n    cli: claude\n    model: {model}\n    system_prompt_file: {system}\n{extra}stages:\n  hold:\n    kind: human_gate\n    capture: text\n    markers:\n      - line: /go\n        then: go\n    on: {{ go: finished }}\n  begin:\n    kind: human_gate\n    capture: text\n    markers:\n      - line: /go\n        then: go\n    on: {{ go: {stage} }}\n  {stage}:\n    kind: agent_turn\n    role: {role}\n    prompt_file: {prompt}\n    capture: json\n{sections}    on: {on}\n  finished:\n    kind: terminal\n"
    );
    (yaml, vec![system, prompt])
}

/// A task parked as if it had just arrived at the probed stage: the history
/// recorded, the workflow state moved, the task stuck so a retry starts the
/// turn. Returns (task id, result ids).
pub(crate) async fn stand_up(server: &TestServer, scenario: &Scenario) -> (String, Vec<String>) {
    let (yaml, files) = probe_workflow(scenario.turn);
    server.write_workflow("probe", &yaml);
    let dir = server
        .builtin_workflow_path("probe")
        .parent()
        .unwrap()
        .to_path_buf();
    for file in files {
        std::fs::copy(prompts_root().join(file), dir.join(file)).unwrap();
    }
    let project: Value = server
        .post("/projects", json!({"name": "probe"}))
        .await
        .json();
    let created: Value = server
        .post(
            "/tasks",
            json!({"project_id": project["id"], "workflow_def": "probe",
                   "title": scenario.title, "prompt": scenario.input,
                   "config": {"cwd": scenario.repo.to_string_lossy()}}),
        )
        .await
        .json();
    let id = created["id"].as_str().expect("task id").to_string();

    let ids = record(server, &id, &scenario.history).await;
    for index in &scenario.prune {
        sqlx::query("DELETE FROM events WHERE id = ?")
            .bind(&ids[*index])
            .execute(server.pool())
            .await
            .unwrap();
    }
    let stage = match scenario.turn {
        Turn::Coder { .. } => "revising",
        _ => "internal_review",
    };
    let state = workflow_state::get(server.pool(), &id)
        .await
        .unwrap()
        .unwrap();
    let mut payload = state.payload.clone();
    payload["current_result_id"] = json!(ids.last().unwrap());
    let (from, outcome) = match scenario.turn {
        Turn::Coder { from, outcome } => (from, outcome),
        _ => ("revising", "done"),
    };
    payload["arrival"] = json!({"from": from, "outcome": outcome});
    workflow_state::update(
        server.pool(),
        &id,
        workflow_state::WorkflowStateUpdate {
            current_stage: stage.to_string(),
            stage_kind: "agent_turn".to_string(),
            loop_counters: state.loop_counters.clone(),
            payload,
            enters_stage: true,
        },
    )
    .await
    .unwrap();
    assert!(
        tasks::mark_stuck(server.pool(), &id, "probe fixture")
            .await
            .unwrap()
    );
    (id, ids)
}

// ---------------------------------------------------------------------------
// The free test: the fixtures are what the real lookup serves
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_probe_fixtures_are_served_by_the_real_lookup() {
    let server = TestServer::start().await;
    let root = server.temp_dir();

    // 1: the previous review and the coder's report, through the anchor.
    let one = mixed_rereview(&root.join("one"));
    let (id, ids) = stand_up(&server, &one).await;
    let binding =
        ContextBinding::new(&id, "s", Some(ids.last().unwrap()), server.base_url()).unwrap();
    let got = via_choco(
        &binding,
        &[
            json!({"source": "task"}),
            json!({"source": "stage", "stage": "internal_review"}),
            json!({"source": "stage", "stage": "revising"}),
            json!({"source": "stage", "stage": "coding"}),
        ],
    )
    .await;
    assert_eq!(got[0].json()["task"]["input"], json!(one.input));
    let review = got[1].json();
    assert_eq!(review["result"]["result_id"], json!(ids[1]));
    assert!(
        review["result"]["capture"]["summary"]
            .as_str()
            .unwrap()
            .contains(&format!("Reviewed: {}", one.commits[1]))
    );
    assert_eq!(got[2].json()["result"]["result_id"], json!(ids[2]));
    assert_eq!(got[3].json()["result"]["result_id"], json!(ids[0]));

    // 2: the ready spec, the human's request and the final coder lap.
    let two = human_scoped_round(&root.join("two"));
    let (id, ids) = stand_up(&server, &two).await;
    let binding =
        ContextBinding::new(&id, "s", Some(ids.last().unwrap()), server.base_url()).unwrap();
    let got = via_choco(
        &binding,
        &[
            json!({"source": "stage", "stage": "spec_check", "outcome": "ready"}),
            json!({"source": "stage", "stage": "awaiting_human_review"}),
            json!({"source": "stage", "stage": "revising"}),
            json!({"source": "stage", "stage": "internal_review"}),
            json!({"source": "stage", "result_id": ids[HUMAN_HANDOFF + 1]}),
        ],
    )
    .await;
    assert_eq!(
        got[0].json()["result"]["capture"]["summary"],
        json!(PLANNER_SPEC)
    );
    let human = got[1].json();
    assert_eq!(human["result"]["source_kind"], "human_gate");
    assert_eq!(human["result"]["result_id"], json!(ids[HUMAN_HANDOFF]));
    assert!(
        human["result"]["capture"]
            .as_str()
            .unwrap()
            .contains("Change nothing else.")
    );
    assert_eq!(got[2].json()["result"]["result_id"], json!(ids[16]));
    assert_eq!(got[3].json()["result"]["result_id"], json!(ids[12]));
    assert_eq!(
        got[4].json()["result"]["previous_result_id"],
        json!(ids[HUMAN_HANDOFF])
    );

    // 3: the pruned hand-off is a named gap, not an empty answer.
    let three = missing_history(&root.join("three"));
    let (id, ids) = stand_up(&server, &three).await;
    let binding =
        ContextBinding::new(&id, "s", Some(ids.last().unwrap()), server.base_url()).unwrap();
    let got = via_choco(
        &binding,
        &[
            json!({"source": "stage", "stage": "revising"}),
            json!({"source": "stage", "stage": "internal_review"}),
            json!({"source": "stage", "stage": "awaiting_human_review"}),
            json!({"source": "stage", "result_id": ids[2]}),
        ],
    )
    .await;
    assert_eq!(got[0].json()["result"]["result_id"], json!(ids[3]));
    assert_eq!(got[1].json()["reason"], "history_gap");
    assert_eq!(got[2].json()["reason"], "history_gap");
    assert_eq!(got[3].json()["reason"], "missing_result");

    // 4: the human's note and the review it answers.
    let four = coder_item_mapping(&root.join("four"));
    let (id, ids) = stand_up(&server, &four).await;
    let binding =
        ContextBinding::new(&id, "s", Some(ids.last().unwrap()), server.base_url()).unwrap();
    let got = via_choco(
        &binding,
        &[
            json!({"source": "stage", "stage": "escalate_to_human"}),
            json!({"source": "stage", "stage": "internal_review"}),
        ],
    )
    .await;
    assert!(
        got[0].json()["result"]["capture"]
            .as_str()
            .unwrap()
            .contains("(F2) Do NOT do")
    );
    assert_eq!(got[1].json()["result"]["result_id"], json!(ids[3]));
}

/// The probes' report checkers on hand-written reports: a right one passes,
/// and each wrong classification fails on its own point. Free: no model.
#[test]
fn the_probe_checkers_tell_right_from_wrong_reports() {
    let head = "5afd54d320ac1858a1f01470362cb61eac63f7e3";
    let right = format!(
        "Reviewed\nReviewed: {head}\n\nFindings\nF1. BLOCKING, repeated. bin/sync.sh:13.\n- `mkdir -p` still runs in dry-run.\nF3. BLOCKING, new code. bin/sync.sh:8.\n- the `break` drops a later -v.\nA2. SERIOUS nonblocking advisory, present at dfc7bde. lib/copy.sh:5: `find -name '*.bak' -delete`.\n- Breaks the never-delete promise. Serious, but it predates the correction.\n\nDismissed\n- none"
    );
    check_mixed_rereview("changes_requested", &right, head).unwrap();
    for (why, wrong) in [
        (
            "old defect made blocking",
            right.replace("SERIOUS nonblocking advisory", "SERIOUS BLOCKING"),
        ),
        (
            "old defect missing",
            right.replace("copy.sh:5", "util.sh:5"),
        ),
        (
            "old defect not serious",
            right
                .replace("Serious, but", "Minor, and")
                .replace("SERIOUS ", ""),
        ),
        (
            "regression nonblocking",
            right.replace(
                "F3. BLOCKING, new code",
                "F3. nonblocking advisory, new code",
            ),
        ),
        (
            "earlier blocker nonblocking",
            right.replace(
                "F1. BLOCKING, repeated",
                "F1. nonblocking advisory, repeated",
            ),
        ),
        ("HEAD not named", right.replace(head, "5afd54d")),
        (
            // A blocking finding elsewhere that happens to say "break" must
            // not stand in for the regression the report made nonblocking.
            "regression nonblocking, an unrelated blocking finding says break",
            right
                .replace(
                    "F3. BLOCKING, new code",
                    "F3. nonblocking advisory, new code",
                )
                .replace(
                    "\n\nDismissed",
                    "\nF4. BLOCKING README.md:3: this wording could break users.\n\nDismissed",
                ),
        ),
        (
            "regression not reported, an unrelated blocking finding says break",
            right
                .replace(
                    "\nF3. BLOCKING, new code. bin/sync.sh:8.\n- the `break` drops a later -v.",
                    "",
                )
                .replace(
                    "\n\nDismissed",
                    "\nF4. BLOCKING README.md:3: this wording could break users.\n\nDismissed",
                ),
        ),
        (
            "both defects folded into one finding",
            right
                .replace(
                    "\nF3. BLOCKING, new code. bin/sync.sh:8.\n- the `break` drops a later -v.",
                    "",
                )
                .replace(
                    "- `mkdir -p` still runs in dry-run.",
                    "- `mkdir -p` still runs in dry-run, and the new `break` drops a later -v.",
                ),
        ),
        (
            "old defect blocks under a second headline",
            right.replace(
                "\n\nDismissed",
                "\nA3. BLOCKING lib/copy.sh:5: the .bak delete.\n\nDismissed",
            ),
        ),
    ] {
        assert!(
            check_mixed_rereview("changes_requested", &wrong, head).is_err(),
            "{why}"
        );
    }
    assert!(check_mixed_rereview("approved", &right, head).is_err());

    assert!(invents_first_review(
        "OK. This is a first review of the branch."
    ));
    assert!(invents_first_review("Treating this as a first review."));
    assert!(invents_first_review("This is my first review of the task."));
    assert!(invents_first_review(
        "It's a first review, so everything is in scope."
    ));
    for fine in [
        "If this is a first review or inside the human's scope, it would block.",
        "I neither treat this as a first review nor approve.",
        "A pruned parent is not evidence of a first review.",
        "Prior findings: unavailable (history_gap).",
    ] {
        assert!(!invents_first_review(fine), "{fine}");
    }

    let human = format!(
        "Findings\n1. BLOCKING README.md: HTTP_RETRY_MAX is not renamed, though the report says it was.\n2. BLOCKING scripts/verify-backoff.sh: recomputes the delays instead of calling retry_delay.\n3. nonblocking: the soak run was accepted by the human, not required.\n\nDismissed\n- jitter. Reviewed: {head}"
    );
    check_human_round("changes_requested", &human, head).unwrap();
    for (why, wrong) in [
        (
            "item 3 nonblocking",
            human.replace("1. BLOCKING README", "1. nonblocking README"),
        ),
        (
            "script nonblocking",
            human.replace("2. BLOCKING scripts", "2. nonblocking scripts"),
        ),
        (
            "soak revived",
            human.replace("3. nonblocking: the soak", "3. BLOCKING: the soak"),
        ),
        (
            // The word "soak" is in the body, not the headline.
            "soak revived in the body of a blocking finding",
            human.replace(
                "3. nonblocking: the soak run was accepted by the human, not required.",
                "3. BLOCKING scripts/run-all.sh: no proof of the retry loop.\n- the 12-hour soak run was never made.",
            ),
        ),
        (
            "item 3 also reported as not blocking",
            human.replace(
                "\n\nDismissed",
                "\n4. nonblocking README.md: RETRY_MAX still appears.\n\nDismissed",
            ),
        ),
    ] {
        assert!(
            check_human_round("changes_requested", &wrong, head).is_err(),
            "{why}"
        );
    }
}

// ---------------------------------------------------------------------------
// The paid probes
// ---------------------------------------------------------------------------

/// What one real turn did, read back from the database.
struct Outcome {
    status: String,
    reported: Option<(String, String)>,
    contexts: Vec<Value>,
    context_errors: usize,
}

async fn run_turn(server: &TestServer, id: &str) -> Outcome {
    let response = server.post_empty(&format!("/tasks/{id}/retry")).await;
    assert_eq!(response.status(), 202, "{}", response.json());
    // Up to ten minutes for the real turn.
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let status = loop {
        let task = server.get(&format!("/tasks/{id}")).await.json();
        let status = task["status"].as_str().unwrap_or("").to_string();
        // A coder's worktree is kept while the task waits at `hold`; a
        // reviewer's ends at `finished`, which removes it.
        let held = task["stage_trail"]
            .as_array()
            .and_then(|trail| trail.last())
            .is_some_and(|entry| entry["payload"]["stage"] == "hold");
        if held || matches!(status.as_str(), "closed" | "stuck" | "cancelled") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = server.post_empty(&format!("/tasks/{id}/cancel")).await;
            panic!("the real turn did not finish in ten minutes (cancelled; output incomplete)");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    let all = events::list_for_task(server.pool(), id).await.unwrap();
    let qualified = chocofactory_core::mcp::qualified_get_context_tool_name();
    let report_name = chocofactory_core::mcp::qualified_report_outcome_tool_name();
    let mut contexts = Vec::new();
    let mut context_errors = 0;
    for event in &all {
        if event.event_type == EventType::ToolCall && event.payload["tool"] == json!(qualified) {
            contexts.push(event.payload["input"].clone());
            let result = all.iter().find(|e| {
                e.event_type == EventType::ToolResult
                    && e.payload["tool_use_id"] == event.payload["tool_use_id"]
            });
            let output = result.map(|e| e.payload["output"].as_str().unwrap_or("").to_string());
            println!(
                "get_context {} -> {}",
                event.payload["input"],
                output
                    .as_deref()
                    .map(|o| o.chars().take(160).collect::<String>())
                    .unwrap_or_else(|| "(no result)".into())
            );
            if result.is_some_and(|e| e.payload["is_error"] == json!(true)) {
                context_errors += 1;
            }
        }
    }
    let reported = all.iter().rev().find_map(|event| {
        (event.event_type == EventType::ToolCall && event.payload["tool"] == json!(report_name))
            .then(|| {
                (
                    event.payload["input"]["outcome"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    event.payload["input"]["summary"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                )
            })
    });
    Outcome {
        status,
        reported,
        contexts,
        context_errors,
    }
}

async fn start_server() -> TestServer {
    let choco = choco_binary();
    TestServer::start_with_adapters(
        |_dir| {
            let claude: Arc<dyn crate::adapter::AgentAdapter> = Arc::new(
                crate::adapter::ClaudeAdapter::new().with_choco_binary(choco.display().to_string()),
            );
            vec![claude]
        },
        &choco.display().to_string(),
    )
    .await
}

fn wanted(name: &str) -> bool {
    std::env::var("CHOCO_REAL_PROBE").as_deref() == Ok(name)
}

/// One finding of a report: its headline line and its whole text. A finding
/// starts at a line that begins with a label such as `F1.`, `A2.`, `(F1)` or
/// `3.`; bullets under it belong to it.
struct Finding {
    headline: String,
    body: String,
}

fn findings(summary: &str) -> Vec<Finding> {
    let start = regex::Regex::new(r"(?mi)^[#*\s]*findings[*:\s]*$").unwrap();
    let end = regex::Regex::new(r"(?mi)^[#*\s]*dismissed[*:\s]*$").unwrap();
    let label = regex::Regex::new(r"^(\(?[A-Za-z]{1,3}\d{1,2}[.)]|\d{1,2}[.)])\s").unwrap();
    let from = start.find(summary).map(|m| m.end()).unwrap_or(0);
    let rest = &summary[from..];
    let rest = end.find(rest).map(|m| &rest[..m.start()]).unwrap_or(rest);
    let mut out: Vec<Finding> = Vec::new();
    for line in rest.lines() {
        if label.is_match(line) || out.is_empty() {
            out.push(Finding {
                headline: line.to_string(),
                body: String::new(),
            });
        }
        let last = out.last_mut().unwrap();
        last.body.push_str(line);
        last.body.push('\n');
    }
    out
}

fn nonblocking(headline: &str) -> bool {
    let h = headline.to_lowercase();
    ["nonblocking", "non-blocking", "not blocking", "advisory"]
        .iter()
        .any(|w| h.contains(w))
}

fn blocking(headline: &str) -> bool {
    headline.to_lowercase().contains("blocking") && !nonblocking(headline)
}

fn has_any(text: &str, words: &[&str]) -> bool {
    let lower = text.to_lowercase();
    words.iter().any(|w| lower.contains(w))
}

/// The indexes of the findings whose HEADLINE names `file` and whose text
/// mentions one of `tokens`. A finding counts for a subject only when it is
/// about that subject's file; a word in some other finding proves nothing.
fn findings_about(all: &[Finding], file: &str, tokens: &[&str]) -> Vec<usize> {
    (0..all.len())
        .filter(|&i| all[i].headline.contains(file) && has_any(&all[i].body, tokens))
        .collect()
}

/// Probe 1's verdict on a report: the unresolved earlier blocker (DEST is
/// still created) and the correction-introduced regression (`--dry-run -v`
/// lost its `-v` output) are separate blocking findings; the old defect in the
/// unchanged callee is prominent, serious and nonblocking.
fn check_mixed_rereview(outcome: &str, summary: &str, head: &str) -> Result<(), String> {
    if outcome != "changes_requested" {
        return Err(format!("outcome {outcome}"));
    }
    if !summary.contains(head) {
        return Err(format!("the summary doesn't name the full HEAD SHA {head}"));
    }
    let all = findings(summary);
    let earlier = findings_about(&all, "sync.sh", &["mkdir"]);
    if earlier.is_empty() {
        return Err("the unresolved earlier blocker (DEST still created) is not reported".into());
    }
    if let Some(&i) = earlier.iter().find(|&&i| !blocking(&all[i].headline)) {
        return Err(format!(
            "the earlier blocker does not block: {}",
            all[i].headline
        ));
    }
    let regression: Vec<usize> = findings_about(&all, "sync.sh", &["-v", "verbose", "break"])
        .into_iter()
        .filter(|i| !earlier.contains(i))
        .collect();
    if regression.is_empty() {
        return Err(
            "the correction-introduced regression is not reported as a finding of its own".into(),
        );
    }
    if let Some(&i) = regression.iter().find(|&&i| !blocking(&all[i].headline)) {
        return Err(format!(
            "the correction-introduced regression does not block: {}",
            all[i].headline
        ));
    }
    let about_copy: Vec<&Finding> = all
        .iter()
        .filter(|f| f.headline.contains("copy.sh"))
        .collect();
    let Some(old) = about_copy.iter().find(|f| f.headline.contains(".bak")) else {
        return Err("the old defect in lib/copy.sh is not reported as a finding".into());
    };
    if let Some(f) = about_copy.iter().find(|f| blocking(&f.headline)) {
        return Err(format!("the old defect blocks: {}", f.headline));
    }
    if !nonblocking(&old.headline) {
        return Err(format!(
            "the old defect is not marked nonblocking: {}",
            old.headline
        ));
    }
    if !has_any(&old.body, &["serious", "severe", "critical", "major"]) {
        return Err(format!(
            "the old defect's seriousness is not stated: {}",
            old.headline
        ));
    }
    Ok(())
}

/// Probe 2's verdict: the false item-3 claim and the defective support
/// script block; the accepted soak run and the jitter demand do not, wherever
/// in a blocking finding they are mentioned.
fn check_human_round(outcome: &str, summary: &str, head: &str) -> Result<(), String> {
    if outcome != "changes_requested" {
        return Err(format!("outcome {outcome}"));
    }
    if !summary.contains(head) {
        return Err(format!("the summary doesn't name the full HEAD SHA {head}"));
    }
    let all = findings(summary);
    let rename: Vec<&Finding> = all
        .iter()
        .filter(|f| {
            has_any(&f.headline, &["readme", "retry_max", "item 3"]) && f.body.contains("RETRY_MAX")
        })
        .collect();
    if !rename.iter().any(|f| blocking(&f.headline)) {
        return Err("item 3 (claimed resolved, not done) does not block".into());
    }
    if let Some(f) = rename.iter().find(|f| !blocking(&f.headline)) {
        return Err(format!(
            "item 3 is also reported as not blocking: {}",
            f.headline
        ));
    }
    let script: Vec<&Finding> = all
        .iter()
        .filter(|f| f.headline.contains("verify-backoff"))
        .collect();
    if !script.iter().any(|f| blocking(&f.headline)) {
        return Err("the defective support script does not block".into());
    }
    if let Some(f) = script.iter().find(|f| !blocking(&f.headline)) {
        return Err(format!(
            "the support script is also reported as not blocking: {}",
            f.headline
        ));
    }
    for f in all.iter().filter(|f| blocking(&f.headline)) {
        let about_work = has_any(&f.headline, &["verify-backoff"])
            || has_any(&f.headline, &["readme", "retry_max", "item 3"])
                && f.body.contains("RETRY_MAX");
        if !about_work && has_any(&f.body, &["soak", "jitter"]) {
            return Err(format!("a revived requirement blocks: {}", f.headline));
        }
    }
    Ok(())
}

/// Whether the human's hand-off was retrieved: by its stage, or by exactly its
/// own result ID. Any other `result_id` is some other result.
fn handoff_retrieved(contexts: &[Value], handoff_id: &str) -> bool {
    contexts
        .iter()
        .any(|c| c["stage"] == "awaiting_human_review" || c["result_id"] == handoff_id)
}

/// The answer on each line that opens with an item's label (`(H1)`, `F2:`,
/// `- **H1**`), lowercased and with the label and its punctuation removed.
fn item_answers(text: &str, item: &str) -> Vec<String> {
    let item = item.to_lowercase();
    text.lines()
        .filter_map(|line| {
            let line = line.to_lowercase();
            let line = line.trim_start_matches(|c: char| !c.is_alphanumeric());
            let rest = line.strip_prefix(item.as_str())?;
            if rest.chars().next().is_some_and(char::is_alphanumeric) {
                return None;
            }
            Some(
                rest.trim_start_matches(|c: char| !c.is_alphanumeric())
                    .to_string(),
            )
        })
        .collect()
}

const NOT_CHANGED: [&str; 7] = [
    "not done",
    "not changed",
    "no change",
    "unchanged",
    "no commit",
    "declined",
    "not implemented",
];

fn names_a_commit(answer: &str, shas: &[String]) -> bool {
    shas.iter().any(|sha| answer.contains(&sha[..7]))
}

/// An answer that opens by saying the item was not changed, and claims no
/// commit: the literal words "no commit" are one way to say it, not the only
/// one, but "changed: added it today; it was not done yesterday" is not one.
fn answers_not_changed(answer: &str, shas: &[String]) -> bool {
    NOT_CHANGED.iter().any(|p| answer.starts_with(p)) && !names_a_commit(answer, shas)
}

/// An answer that does not open with "not changed" and names a real commit.
fn answers_changed(answer: &str, commits_for_the_item: &[String]) -> bool {
    !NOT_CHANGED.iter().any(|p| answer.starts_with(p))
        && names_a_commit(answer, commits_for_the_item)
}

/// The coder's item mapping, in one text (the work report, or the
/// report_outcome summary): H1 and F1 changed, each naming a real commit that
/// touches its own file; H2 and F2 not changed, each saying so and naming no
/// commit.
fn check_item_answers(
    text: &str,
    h1: &[String],
    f1: &[String],
    all: &[String],
) -> Result<(), String> {
    for (item, real) in [("H1", h1), ("F1", f1)] {
        if !item_answers(text, item)
            .iter()
            .any(|a| answers_changed(a, real))
        {
            return Err(format!(
                "{item}: no line answers it as changed with a real commit touching its file ({real:?})"
            ));
        }
    }
    for item in ["H2", "F2"] {
        if !item_answers(text, item)
            .iter()
            .any(|a| answers_not_changed(a, all))
        {
            return Err(format!("{item}: no line answers it as not changed"));
        }
    }
    Ok(())
}

/// Whether a report affirms "this is a first review". A conditional ("if
/// this is a first review") or a refusal ("I do not treat this as a first
/// review") is not an affirmation.
fn invents_first_review(summary: &str) -> bool {
    let lower = summary.to_lowercase();
    lower.match_indices("first review").any(|(at, _)| {
        let mut from = at.saturating_sub(40);
        while !lower.is_char_boundary(from) {
            from -= 1;
        }
        let before = &lower[from..at];
        let affirms = [
            "this is a ",
            "this as a ",
            "this was a ",
            "this is the ",
            "this is my ",
            "this is our ",
            "it is a ",
            "it's a ",
            "treating this as ",
            "treat this as ",
        ]
        .iter()
        .any(|p| before.ends_with(p));
        let hedged = [
            "if ", "whether", "not ", "n't", "neither", "nor ", "never", "unless",
        ]
        .iter()
        .any(|w| before.contains(w));
        affirms && !hedged
    })
}

/// The coder probe's checks, on hand-written reports. Free: no model.
#[test]
fn the_coder_item_checks_accept_an_answer_only_for_its_own_item() {
    let h1 = vec!["1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()];
    let f1 = vec!["2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()];
    let all: Vec<String> = h1.iter().chain(&f1).cloned().collect();
    // The accepted third run's shape: F2 declined without the words "no commit".
    let right = "Work report\n(H1) changed, commit 1111111: exits 2 when SUBJECT is missing.\n(H2) not changed: --help already printed the usage line; no commit.\n(F1) changed, commit 2222222: the subject is quoted.\n(F2) not done and no proof added, as you instructed.\n";
    check_item_answers(right, &h1, &f1, &all).unwrap();
    assert_eq!(
        item_answers(right, "F2"),
        vec!["not done and no proof added, as you instructed.".to_string()]
    );
    // Bullets and bold labels are still labels.
    check_item_answers(
        &right.replace("(H1)", "- **H1**").replace("(F2)", "* F2:"),
        &h1,
        &f1,
        &all,
    )
    .unwrap();
    for (why, wrong) in [
        (
            // "changed" is the answer; "not done" is about yesterday.
            "an affirmative answer that mentions 'not done' later",
            right.replace(
                "(F2) not done and no proof added, as you instructed.",
                "(F2) changed: added soak proof today; it was not done yesterday",
            ),
        ),
        (
            "a not-changed answer that names a real commit",
            right.replace(
                "not done and no proof added",
                "not done, but see commit 2222222",
            ),
        ),
        (
            "the answer sits under another item's label",
            right.replace("(F2) not done", "(F3) not done"),
        ),
        (
            "an item that only mentions its label mid-line",
            right.replace(
                "(F2) not done and no proof added, as you instructed.",
                "see F2: not done and no proof added",
            ),
        ),
        (
            "H1 names a commit that does not touch its file",
            right.replace("commit 1111111", "commit 2222222"),
        ),
        (
            "H1 says not changed",
            right.replace(
                "(H1) changed, commit 1111111:",
                "(H1) not changed, commit 1111111:",
            ),
        ),
        (
            "quoted policy instead of item answers",
            "For each item say whether it changed or not, why, and the commit. H1 F1 H2 F2. Nothing else."
                .to_string(),
        ),
    ] {
        assert!(check_item_answers(&wrong, &h1, &f1, &all).is_err(), "{why}");
    }
}

/// The human's hand-off counts as retrieved only by its stage or by its own ID.
#[test]
fn the_handoff_is_retrieved_only_by_its_stage_or_its_own_result_id() {
    let handoff = "id-of-the-handoff";
    assert!(handoff_retrieved(
        &[json!({"source": "stage", "stage": "awaiting_human_review"})],
        handoff
    ));
    assert!(handoff_retrieved(
        &[
            json!({"source": "task"}),
            json!({"source": "stage", "result_id": handoff})
        ],
        handoff
    ));
    assert!(!handoff_retrieved(
        &[
            json!({"source": "stage", "result_id": "some-other-result"}),
            json!({"source": "stage", "stage": "internal_review"}),
        ],
        handoff
    ));
    assert!(!handoff_retrieved(&[], handoff));
}

/// Runs `bash <script> <args>` in `dir`, returning the exit status and the
/// whole of stdout.
fn run_bash(dir: &Path, script: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new("bash")
        .arg(dir.join(script))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run bash");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// The mixed re-review's fixture holds what its report says, observably, by
/// running the real scripts at the real commits: the earlier blocker is still
/// there after the correction, the old defect was already there at the reviewed
/// commit, and the correction really changed `--dry-run -v`. Free: no model.
#[test]
fn the_mixed_fixture_regression_and_old_defect_are_observable() {
    let dir = std::env::temp_dir().join(format!("probe-mixed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("repo");
    let scenario = mixed_rereview(&repo);
    let (reviewed, correction) = (&scenario.commits[1], &scenario.commits[3]);
    // A tree at a commit: bin/sync.sh and lib/copy.sh as they were there.
    let at = |name: &str, sha: &str| -> PathBuf {
        let tree = dir.join(name);
        for file in ["bin/sync.sh", "lib/copy.sh"] {
            let body = git(&repo, &["show", &format!("{sha}:{file}")]) + "\n";
            std::fs::create_dir_all(tree.join(file).parent().unwrap()).unwrap();
            std::fs::write(tree.join(file), body).unwrap();
        }
        let src = tree.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(src.join(f), "x").unwrap();
        }
        tree
    };
    let before = at("before", reviewed);
    let after = at("after", correction);
    let sync = |tree: &Path, flags: &[&str]| {
        let mut args = vec!["src", "dest"];
        args.extend_from_slice(flags);
        let _ = std::fs::remove_dir_all(tree.join("dest"));
        let result = run_bash(tree, "bin/sync.sh", &args);
        (result, tree.join("dest").exists())
    };

    // The regression: `--dry-run -v` printed the source of each file at the
    // reviewed commit, and no longer does after the correction.
    let (b, _) = sync(&before, &["--dry-run", "-v"]);
    let (a, _) = sync(&after, &["--dry-run", "-v"]);
    assert_eq!(b.0, Some(0));
    assert_eq!(a.0, Some(0));
    assert!(
        b.1.contains("  from src/a.txt") && b.1.contains("  from src/b.txt"),
        "{}",
        b.1
    );
    assert!(!a.1.contains("  from"), "{}", a.1);
    // The same flags in the other order, and without -v, are unchanged: the
    // lost output is the correction's own doing, not an old difference.
    assert_eq!(
        sync(&before, &["-v", "--dry-run"]).0.1,
        sync(&after, &["-v", "--dry-run"]).0.1
    );
    assert_eq!(
        sync(&before, &["--dry-run"]).0.1,
        sync(&after, &["--dry-run"]).0.1
    );
    assert_eq!(
        sync(&before, &["--dry-run"]).0.1,
        "would copy a.txt\nwould copy b.txt\n"
    );

    // The earlier blocker: a dry run still creates DEST, before and after.
    assert!(
        sync(&before, &["--dry-run"]).1,
        "DEST at the reviewed commit"
    );
    assert!(sync(&after, &["--dry-run"]).1, "DEST after the correction");

    // The old defect: copy.sh deletes a `*.bak` already in DEST, at the
    // reviewed commit and after, and the correction did not touch the file.
    for tree in [&before, &after] {
        let _ = std::fs::remove_dir_all(tree.join("dest"));
        std::fs::create_dir_all(tree.join("dest")).unwrap();
        std::fs::write(tree.join("dest/keep.bak"), "precious").unwrap();
        let (status, _) = run_bash(tree, "bin/sync.sh", &["src", "dest"]);
        assert_eq!(status, Some(0));
        assert!(
            !tree.join("dest/keep.bak").exists(),
            "the old defect is gone"
        );
    }
    assert_eq!(
        git(
            &repo,
            &[
                "diff",
                "--name-only",
                &format!("{reviewed}..{correction}"),
                "--",
                "lib/copy.sh"
            ]
        ),
        ""
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The coder fixture starts with exactly the faults the probe checks for.
#[test]
fn the_coder_fixture_starts_broken_where_the_probe_expects() {
    let dir = std::env::temp_dir().join(format!("probe-coder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let scenario = coder_item_mapping(&dir.join("repo"));
    let repo = &scenario.repo;
    assert_eq!(
        run_bash(repo, "bin/notify.sh", &[]).0,
        Some(1),
        "H1 starts broken"
    );
    assert_eq!(
        run_bash(repo, "bin/notify.sh", &["--help"]),
        (Some(0), "usage: notify.sh SUBJECT [BODY]\n".to_string()),
        "H2 starts done"
    );
    assert_ne!(
        run_bash(repo, "bin/notify.sh", &["two  spaces", "body"]).1,
        "[notify] two  spaces\nbody\n",
        "F1 starts broken"
    );
    let format = std::fs::read_to_string(repo.join("lib/format.sh")).unwrap();
    assert_eq!(format.matches("msg").count(), 3);
    std::fs::remove_dir_all(&dir).unwrap();
}

fn assert_bounded(contexts: &[Value]) {
    assert!(!contexts.is_empty(), "the agent never called get_context");
    assert!(contexts.len() <= 16, "{} get_context calls", contexts.len());
    for call in contexts {
        let repeats = contexts.iter().filter(|c| *c == call).count();
        assert!(repeats <= 2, "the same lookup {call} ran {repeats} times");
    }
}

#[tokio::test]
#[ignore = "paid: one real Opus 5.5 review; CHOCO_REAL_PROBE=mixed_rereview"]
async fn probe_mixed_rereview() {
    if !wanted("mixed_rereview") {
        return;
    }
    let server = start_server().await;
    let scenario = mixed_rereview(&server.temp_dir().join("repo"));
    let (id, _) = stand_up(&server, &scenario).await;
    let out = run_turn(&server, &id).await;
    let (outcome, summary) = out.reported.clone().expect("the reviewer never reported");
    println!("OUTCOME {outcome}\n{summary}");
    assert_bounded(&out.contexts);
    assert!(
        out.contexts.iter().any(|c| c == &json!({"source": "task"})),
        "no task lookup: {:?}",
        out.contexts
    );
    assert!(
        out.contexts.iter().any(|c| c["stage"] == "internal_review"),
        "the previous review was never looked up: {:?}",
        out.contexts
    );
    let head = scenario.commits.last().unwrap();
    if let Err(why) = check_mixed_rereview(&outcome, &summary, head) {
        panic!("{why}");
    }
    assert_eq!(out.status, "closed");
}

#[tokio::test]
#[ignore = "paid: one real Opus 5.5 review; CHOCO_REAL_PROBE=human_scoped_round"]
async fn probe_human_scoped_round() {
    if !wanted("human_scoped_round") {
        return;
    }
    let server = start_server().await;
    let scenario = human_scoped_round(&server.temp_dir().join("repo"));
    let (id, ids) = stand_up(&server, &scenario).await;
    let out = run_turn(&server, &id).await;
    let (outcome, summary) = out.reported.clone().expect("the reviewer never reported");
    println!("OUTCOME {outcome}\n{summary}");
    assert_bounded(&out.contexts);
    assert!(
        out.contexts
            .iter()
            .any(|c| c["stage"] == "spec_check" && c["outcome"] == "ready"),
        "the ready spec was never looked up: {:?}",
        out.contexts
    );
    assert!(
        handoff_retrieved(&out.contexts, &ids[HUMAN_HANDOFF]),
        "the human's hand-off ({}) was never retrieved: {:?}",
        ids[HUMAN_HANDOFF],
        out.contexts
    );
    let head = scenario.commits.last().unwrap();
    if let Err(why) = check_human_round(&outcome, &summary, head) {
        panic!("{why}");
    }
    assert_eq!(out.status, "closed");
}

#[tokio::test]
#[ignore = "paid: one real Opus 5.5 review; CHOCO_REAL_PROBE=missing_history"]
async fn probe_missing_history() {
    if !wanted("missing_history") {
        return;
    }
    let server = start_server().await;
    let scenario = missing_history(&server.temp_dir().join("repo"));
    let (id, _) = stand_up(&server, &scenario).await;
    let out = run_turn(&server, &id).await;
    let (outcome, summary) = out.reported.clone().expect("the reviewer never reported");
    println!("OUTCOME {outcome}\n{summary}");
    assert_bounded(&out.contexts);
    assert_eq!(
        outcome, "changes_requested",
        "an unavailable governing hand-off must hold approval"
    );
    let lower = summary.to_lowercase();
    assert!(
        lower.contains("history_gap") || lower.contains("pruned") || lower.contains("unavailable"),
        "the limitation is not disclosed: {summary}"
    );
    assert!(
        lower.contains("human")
            || lower.contains("awaiting_human_review")
            || lower.contains("hand-off")
            || lower.contains("handoff"),
        "the missing hand-off is not named: {summary}"
    );
    assert!(
        !invents_first_review(&summary),
        "invented a first review: {summary}"
    );
    assert_eq!(out.status, "closed");
}

#[tokio::test]
#[ignore = "paid: one real Sonnet 5.5 coder turn; CHOCO_REAL_PROBE=coder_item_mapping"]
async fn probe_coder_item_mapping() {
    if !wanted("coder_item_mapping") {
        return;
    }
    let server = start_server().await;
    let scenario = coder_item_mapping(&server.temp_dir().join("repo"));
    let (id, _) = stand_up(&server, &scenario).await;
    let out = run_turn(&server, &id).await;
    let (outcome, summary) = out.reported.clone().expect("the coder never reported");
    println!("OUTCOME {outcome}\n{summary}");
    assert_bounded(&out.contexts);
    assert_eq!(outcome, "done");
    // The task's worktree: its branch, its commits and its work report.
    let task = server.get(&format!("/tasks/{id}")).await.json();
    let worktree = crate::worktree::worktree_path(
        Path::new(task["worktree_repo"].as_str().expect("worktree repo")),
        task["worktree_project"].as_str().expect("worktree project"),
        &id,
    )
    .unwrap();
    let base = &scenario.commits[0];
    let log = git(&worktree, &["log", "--format=%H", &format!("{base}..HEAD")]);
    let shas: Vec<&str> = log.lines().collect();
    assert!(!shas.is_empty(), "the coder made no commit");
    let touching = |path: &str| -> Vec<String> {
        git(
            &worktree,
            &["log", "--format=%H", &format!("{base}..HEAD"), "--", path],
        )
        .lines()
        .map(str::to_string)
        .collect()
    };
    // Behavior, by running the scripts the coder left, not by reading them.
    let run = |args: &[&str]| -> (Option<i32>, String) {
        let out = Command::new("bash")
            .arg(worktree.join("bin/notify.sh"))
            .args(args)
            .current_dir(&worktree)
            .output()
            .expect("run notify.sh");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    assert_eq!(run(&[]).0, Some(2), "H1: a missing SUBJECT must exit 2");
    assert_eq!(
        run(&["--help"]),
        (Some(0), "usage: notify.sh SUBJECT [BODY]\n".to_string()),
        "H2 broke"
    );
    assert_eq!(
        run(&["two  spaces", "body"]),
        (Some(0), "[notify] two  spaces\nbody\n".to_string()),
        "F1: the subject is still word-split"
    );
    // No edit for the advisory (renaming `msg`) and no soak proof anywhere.
    let format = std::fs::read_to_string(worktree.join("lib/format.sh")).unwrap();
    assert_eq!(
        format.matches("msg").count(),
        3,
        "the style advisory was edited:\n{format}"
    );
    assert!(!format.contains("message"), "the style advisory was edited");
    let tracked = git(&worktree, &["ls-files"]);
    assert!(
        !tracked.to_lowercase().contains("soak"),
        "a soak proof was added:\n{tracked}"
    );
    let git_dir = git(&worktree, &["rev-parse", "--absolute-git-dir"]);
    let report = std::fs::read_to_string(Path::new(&git_dir).join("choco-work-report.md"))
        .expect("the work report was not written");
    // The same item answers in the work report and in report_outcome: H1 and
    // F1 changed with a real commit touching their file, H2 and F2 not changed.
    let (h1, f1) = (touching("bin/notify.sh"), touching("lib/format.sh"));
    let all: Vec<String> = shas.iter().map(|s| s.to_string()).collect();
    for (label, text) in [
        ("work report", report.as_str()),
        ("report_outcome summary", summary.as_str()),
    ] {
        if let Err(why) = check_item_answers(text, &h1, &f1, &all) {
            panic!("{label}: {why}\n{text}");
        }
    }
    assert_eq!(out.context_errors, 0);
    assert_eq!(out.status, "open", "the coder did not leave the stage");
}
