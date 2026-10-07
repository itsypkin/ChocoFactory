//! Direct coverage for `workflows/scripts/ci-checks.sh`.
//!
//! `checks_polling` routes the whole CI stage from that script's first line.
//! This file runs the shipped script inside a temporary git repository with a
//! fake `gh` first on `PATH`. The fake applies each call's `-q` filter with
//! `jq` to canned JSON, standing in for the filter engine inside `gh`.
//! The same case table (`ci-checks-cases.json`) is read by a unit test in
//! `config_root.rs` that runs the first lines through the seeded workflow.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_GH: &str = r#"#!/bin/sh
# $DIR/states: one check state per line. Switches (files in $DIR):
# fail-view, fail-checks, empty-checks, empty-head, count (overrides the count),
# fail-rerun (`run rerun` exits 1). $DIR/links: one link per check, in states
# order; an empty line is "", the line <none> leaves the field out.
DIR="$GH_FAKE_DIR"
echo "$*" >> "$DIR/calls"
q=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-q" ]; then q=$a; fi
    prev=$a
done
if [ "$1" = run ] && [ "$2" = rerun ]; then
    [ -e "$DIR/fail-rerun" ] && { echo "fake gh: run rerun failed" >&2; exit 1; }
    echo "fake gh: requested rerun"
    exit 0
fi
[ "$1" = pr ] || { echo "fake gh: unhandled: $*" >&2; exit 1; }
case "$2" in
view)
    [ -e "$DIR/fail-view" ] && { echo "fake gh: pr view failed" >&2; exit 1; }
    head=0123456789abcdef
    [ -e "$DIR/empty-head" ] && head=""
    rollup=$(jq -R -s -c 'split("\n") | map(select(. != "")) | map({state: .})' < "$DIR/states")
    json=$(printf '{"headRefOid":"%s","statusCheckRollup":%s}' "$head" "$rollup")
    if [ -e "$DIR/count" ]; then
        json=$(printf '%s' "$json" | jq -c ".statusCheckRollup = $(cat "$DIR/count")")
        # A count that is not a list: `length` of it is the value itself for
        # numbers, and the string's length otherwise, so emit it directly.
        printf '%s\n%s\n' "$head" "$(cat "$DIR/count" | tr -d '"')"
        exit 0
    fi
    printf '%s' "$json" | jq -r "$q"
    ;;
checks)
    [ -e "$DIR/fail-checks" ] && { echo "fake gh: pr checks failed" >&2; exit 1; }
    [ -e "$DIR/empty-checks" ] && exit 0
    lf="$DIR/links"
    if [ ! -e "$lf" ]; then
        lf="$DIR/links.default"
        awk '{ printf "https://github.com/o/r/actions/runs/100/job/%d\n", NR - 1 }' "$DIR/states" > "$lf"
    fi
    jq -R -s -c --rawfile links "$lf" '($links | split("\n")) as $l | split("\n") | map(select(. != "")) | to_entries | map({name: ("job-" + (.key | tostring)), state: .value} + (if $l[.key] == "<none>" then {} else {link: $l[.key]} end))' < "$DIR/states" | jq -r "$q"
    exit 0
    ;;
*) echo "fake gh: unhandled: $*" >&2; exit 1 ;;
esac
"#;

const HEAD: &str = "0123456789abcdef";

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new(states: &[&str]) -> Self {
        use std::os::unix::fs::PermissionsExt;
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ci-checks-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).unwrap();
        // A `date` that honours FAKE_NOW, so the grace boundary is exact.
        let date = dir.join("date");
        fs::write(
            &date,
            "#!/bin/sh\n[ -e \"$GH_FAKE_DIR/date-fail\" ] && exit 1\n[ -e \"$GH_FAKE_DIR/date-junk\" ] && { echo soon; exit 0; }\nif [ -n \"${FAKE_NOW:-}\" ]; then echo \"$FAKE_NOW\"; else exec /bin/date \"$@\"; fi\n",
        )
        .unwrap();
        fs::set_permissions(&date, fs::Permissions::from_mode(0o755)).unwrap();
        let gh = dir.join("gh");
        fs::write(&gh, FAKE_GH).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let fx = Fixture { dir, repo };
        fx.set_states(states);
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&fx.repo)
            .status()
            .unwrap();
        assert!(init.success());
        fx
    }

    fn set_states(&self, states: &[&str]) {
        let mut text = states.join("\n");
        text.push('\n');
        fs::write(self.dir.join("states"), text).unwrap();
    }

    fn set_links(&self, links: &[&str]) {
        fs::write(self.dir.join("links"), format!("{}\n", links.join("\n"))).unwrap();
    }

    fn record_path(&self) -> PathBuf {
        self.repo.join(".git/choco-ci-rerun")
    }

    fn record(&self) -> Option<String> {
        fs::read_to_string(self.record_path()).ok()
    }

    /// The `run rerun` lines of the fake's call log.
    fn reruns(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("run rerun"))
            .map(str::to_string)
            .collect()
    }

    fn switch(&self, name: &str) {
        fs::write(self.dir.join(name), "").unwrap();
    }

    /// Puts a fake `name` on PATH that exits with `code` and prints nothing.
    fn broken_tool(&self, name: &str, code: i32) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.dir.join(name);
        fs::write(&p, format!("#!/bin/sh\nexit {code}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn first_seen(&self) -> PathBuf {
        self.repo.join(".git/choco-ci-first-seen")
    }

    fn write_first_seen(&self, text: &str) {
        fs::write(self.first_seen(), text).unwrap();
    }

    fn read_first_seen(&self) -> String {
        fs::read_to_string(self.first_seen()).unwrap()
    }

    fn run_in(&self, cwd: &Path) -> Output {
        self.run_at(cwd, None)
    }

    fn run_at(&self, cwd: &Path, now: Option<u64>) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/scripts/ci-checks.sh"))
            .current_dir(cwd)
            .env("PATH", path)
            .env("GH_FAKE_DIR", &self.dir)
            .env("GIT_CEILING_DIRECTORIES", &self.dir)
            .env("PR_NUMBER", "7")
            .env("FAKE_NOW", now.map(|n| n.to_string()).unwrap_or_default())
            .output()
            .expect("failed to run ci-checks.sh")
    }

    fn run(&self) -> Output {
        self.run_in(&self.repo)
    }

    /// Runs and expects success; returns stdout.
    fn ok(&self) -> String {
        let out = self.run();
        assert!(
            out.status.success(),
            "script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs and expects the fail-closed shape: non-zero, nothing on stdout,
    /// a note on stderr. Returns stderr.
    fn fails(&self) -> String {
        let out = self.run();
        assert!(!out.status.success(), "expected a failure");
        assert!(
            out.stdout.is_empty(),
            "stdout must be empty: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("choco ci-checks: "), "stderr: {err}");
        err
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn first_line(out: &str) -> &str {
    out.lines().next().unwrap_or("")
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    states: Vec<String>,
    #[serde(default)]
    links: Option<Vec<String>>,
    first_line: String,
}

#[test]
fn every_shared_case_prints_its_token_and_the_sorted_check_lines() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("fixtures/ci-checks-cases.json")).unwrap();
    assert!(cases.len() >= 20);
    for case in cases {
        let states: Vec<&str> = case.states.iter().map(String::as_str).collect();
        let fx = Fixture::new(&states);
        if let Some(links) = &case.links {
            assert_eq!(links.len(), states.len(), "case '{}'", case.name);
            let links: Vec<&str> = links.iter().map(String::as_str).collect();
            fx.set_links(&links);
        }
        let out = fx.ok();
        assert_eq!(first_line(&out), case.first_line, "case '{}'", case.name);
        let mut lines: Vec<String> = case
            .states
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{s} job-{i}"))
            .collect();
        lines.sort();
        let expected = format!("{}\n\n{}\n", case.first_line, lines.join("\n"));
        let has_failure = case
            .states
            .iter()
            .any(|s| matches!(s.as_str(), "FAILURE" | "ERROR" | "TIMED_OUT"));
        if has_failure {
            // A failure may add a note after the check lines.
            assert!(out.starts_with(&expected), "case '{}': {out}", case.name);
        } else {
            assert_eq!(out, expected, "case '{}'", case.name);
        }
        assert!(!fx.first_seen().exists(), "case '{}'", case.name);
    }
}

#[test]
fn pr_view_and_pr_checks_are_called_the_documented_way() {
    let fx = Fixture::new(&["SUCCESS"]);
    fx.ok();
    let calls = fs::read_to_string(fx.dir.join("calls")).unwrap();
    assert!(
        calls.contains("pr view 7 --json headRefOid,statusCheckRollup"),
        "{calls}"
    );
    assert!(
        calls.contains("pr checks 7 --json name,state,link"),
        "{calls}"
    );
}

#[test]
fn no_checks_before_the_grace_starts_the_clock_and_keeps_polling() {
    let fx = Fixture::new(&[]);
    let out = fx.ok();
    assert_eq!(first_line(&out), "PENDING");
    assert!(out.contains(HEAD), "{out}");
    let seen = fx.read_first_seen();
    let mut parts = seen.split_whitespace();
    assert_eq!(parts.next(), Some(HEAD));
    assert!(parts.next().unwrap().parse::<u64>().is_ok());
    assert_eq!(parts.next(), None);

    assert_eq!(first_line(&fx.ok()), "PENDING");
    assert_eq!(fx.read_first_seen(), seen, "a second run keeps the clock");
}

#[test]
fn no_checks_after_the_grace_is_no_checks() {
    let t = 1_800_000_000u64;
    for (age, token) in [
        (181, "NO_CHECKS"),
        (180, "NO_CHECKS"),
        (179, "PENDING"),
        (30, "PENDING"),
    ] {
        let fx = Fixture::new(&[]);
        fx.write_first_seen(&format!("{HEAD} {}\n", t - age));
        let out = fx.run_at(&fx.repo, Some(t));
        assert!(out.status.success());
        let out = String::from_utf8(out.stdout).unwrap();
        assert_eq!(first_line(&out), token, "age {age}");
        if token == "NO_CHECKS" {
            assert!(
                out.contains(&format!(
                    "No CI checks reported on this PR's head {HEAD} after {age}s."
                )),
                "{out}"
            );
        }
    }
}

#[test]
fn checks_that_register_late_beat_an_old_first_seen_record() {
    for (state, token) in [("SUCCESS", "GREEN"), ("IN_PROGRESS", "PENDING")] {
        let fx = Fixture::new(&[state]);
        fx.write_first_seen(&format!("{HEAD} {}\n", now() - 10_000));
        assert_eq!(first_line(&fx.ok()), token);
    }
}

#[test]
fn a_new_head_resets_the_clock() {
    let fx = Fixture::new(&[]);
    fx.write_first_seen(&format!("fedcba9876543210 {}\n", now() - 10_000));
    assert_eq!(first_line(&fx.ok()), "PENDING");
    assert!(fx.read_first_seen().starts_with(&format!("{HEAD} ")));
}

#[test]
fn an_unparsable_first_seen_file_is_rewritten_with_a_note() {
    for junk in [
        "garbage\n",
        "",
        "sha notanumber\n",
        &format!("{HEAD} 1 extra\n"),
    ] {
        let fx = Fixture::new(&[]);
        fx.write_first_seen(junk);
        let out = fx.run();
        assert!(out.status.success());
        assert_eq!(
            first_line(&String::from_utf8(out.stdout).unwrap()),
            "PENDING"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("choco ci-checks: "),
            "junk {junk:?}"
        );
        assert!(fx.read_first_seen().starts_with(&format!("{HEAD} ")));
    }
}

#[test]
fn a_failing_pr_view_fails_closed() {
    let fx = Fixture::new(&["SUCCESS"]);
    fx.switch("fail-view");
    assert!(fx.fails().contains("gh pr view failed"));
}

#[test]
fn an_empty_head_fails_closed() {
    let fx = Fixture::new(&[]);
    fx.switch("empty-head");
    assert!(fx.fails().contains("no head commit"));
}

#[test]
fn a_non_numeric_count_fails_closed() {
    let fx = Fixture::new(&[]);
    fs::write(fx.dir.join("count"), "\"lots\"").unwrap();
    assert!(fx.fails().contains("not a number"));
}

#[test]
fn a_failing_pr_checks_fails_closed() {
    let fx = Fixture::new(&["SUCCESS"]);
    fx.switch("fail-checks");
    assert!(fx.fails().contains("gh pr checks failed"));
}

#[test]
fn an_empty_pr_checks_with_a_positive_count_fails_closed() {
    let fx = Fixture::new(&["SUCCESS"]);
    fx.switch("empty-checks");
    assert!(fx.fails().contains("printed nothing"));
}

#[test]
fn no_git_directory_with_no_checks_fails_closed() {
    let fx = Fixture::new(&[]);
    let bare = fx.dir.join("not-a-repo");
    fs::create_dir_all(&bare).unwrap();
    let out = fx.run_in(&bare);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("choco ci-checks: "));
}

#[test]
fn an_unwritable_first_seen_file_fails_closed() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new(&[]);
    let git_dir = fx.repo.join(".git");
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let writable = fs::write(git_dir.join("probe"), "").is_ok();
    let out = fx.run();
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // Running as a user the mode does not bind (root): nothing to test.
        return;
    }
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("choco ci-checks: "));
}

#[test]
fn a_failing_awk_never_turns_a_red_pr_into_green() {
    let fx = Fixture::new(&["FAILURE", "PENDING"]);
    fx.broken_tool("awk", 2);
    assert!(fx.fails().contains("awk failed"));
}

#[test]
fn a_failing_run_id_extraction_fails_closed_without_rerunning() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new(&["FAILURE", "SUCCESS"]);
    // Passes every sed call except the `-n` run-id extraction.
    let p = fx.dir.join("sed");
    fs::write(
        &p,
        "#!/bin/sh\nif [ \"$1\" = -n ]; then exit 2; fi\nexec /usr/bin/sed \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fx.fails().contains("sed failed"));
    assert!(
        !fs::read_to_string(fx.dir.join("calls"))
            .unwrap_or_default()
            .contains("run rerun")
    );
}

#[test]
fn a_failing_sort_fails_closed() {
    let fx = Fixture::new(&["FAILURE", "PENDING"]);
    fx.broken_tool("sort", 2);
    assert!(fx.fails().contains("sort failed"));
}

#[test]
fn a_failing_date_fails_closed() {
    let fx = Fixture::new(&[]);
    fx.switch("date-fail");
    assert!(fx.fails().contains("date failed"));
    assert!(!fx.first_seen().exists());
}

#[test]
fn a_date_that_is_not_a_number_fails_closed() {
    let fx = Fixture::new(&[]);
    fx.switch("date-junk");
    assert!(fx.fails().contains("not a number"));
    assert!(!fx.first_seen().exists());
}

#[test]
fn a_failing_move_of_the_first_seen_file_fails_closed() {
    let fx = Fixture::new(&[]);
    fx.broken_tool("mv", 1);
    assert!(fx.fails().contains("cannot move"));
    assert!(!fx.first_seen().exists());
}

#[test]
fn a_missing_pr_number_fails_closed() {
    let fx = Fixture::new(&["SUCCESS"]);
    let out = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/scripts/ci-checks.sh"))
        .current_dir(&fx.repo)
        .env_remove("PR_NUMBER")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("PR_NUMBER is not set"));
}

// ---- re-run of failed Actions jobs ----

const L100: &str = "https://github.com/o/r/actions/runs/100/job/";
const L200: &str = "https://github.com/o/r/actions/runs/200/job/";

fn link(base: &str, n: u32) -> String {
    format!("{base}{n}")
}

fn out_and_err(fx: &Fixture) -> (String, String) {
    let out = fx.run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn the_first_red_reruns_each_failed_run_once_and_records_the_links() {
    let fx = Fixture::new(&["FAILURE", "FAILURE", "SUCCESS", "ERROR"]);
    let (a, b, c) = (link(L100, 1), link(L100, 2), link(L200, 3));
    fx.set_links(&[&a, &b, "https://github.com/o/r/actions/runs/100/job/9", &c]);
    let out = fx.ok();
    assert_eq!(first_line(&out), "PENDING");
    assert!(
        out.ends_with(&format!(
            "\nRe-ran the failed jobs of run 100 200 for {HEAD}.\n"
        )),
        "{out}"
    );
    assert_eq!(
        fx.reruns(),
        vec!["run rerun 100 --failed", "run rerun 200 --failed"]
    );
    assert_eq!(fx.record().unwrap(), format!("{HEAD}\n{a}\n{b}\n{c}\n"));
}

#[test]
fn a_recorded_failure_waits_for_the_rerun_without_rerunning_again() {
    let fx = Fixture::new(&["FAILURE", "SUCCESS"]);
    let a = link(L100, 0);
    fs::write(fx.record_path(), format!("{HEAD}\n{a}\n")).unwrap();
    let out = fx.ok();
    assert_eq!(first_line(&out), "PENDING");
    assert!(
        out.ends_with(&format!(
            "\nWaiting for the re-run of the failed jobs for {HEAD}.\n"
        )),
        "{out}"
    );
    assert!(fx.reruns().is_empty());
}

#[test]
fn a_failure_not_in_the_record_is_red_without_another_rerun() {
    let fx = Fixture::new(&["FAILURE", "FAILURE"]);
    fs::write(fx.record_path(), format!("{HEAD}\n{}\n", link(L100, 0))).unwrap();
    let out = fx.ok();
    assert_eq!(out, format!("RED\n\nFAILURE job-0\nFAILURE job-1\n"));
    assert!(fx.reruns().is_empty());
}

#[test]
fn a_green_rerun_is_green() {
    let fx = Fixture::new(&["SUCCESS", "SUCCESS"]);
    fs::write(fx.record_path(), format!("{HEAD}\n{}\n", link(L100, 0))).unwrap();
    assert_eq!(first_line(&fx.ok()), "GREEN");
    assert!(fx.reruns().is_empty());
}

#[test]
fn a_record_for_another_head_is_replaced_by_a_new_rerun() {
    let fx = Fixture::new(&["FAILURE"]);
    fs::write(
        fx.record_path(),
        format!("fedcba9876543210\n{}\n", link(L100, 0)),
    )
    .unwrap();
    let (out, err) = out_and_err(&fx);
    assert_eq!(first_line(&out), "PENDING");
    assert!(err.is_empty(), "no note for another sha: {err}");
    assert_eq!(fx.reruns(), vec!["run rerun 100 --failed"]);
    assert_eq!(fx.record().unwrap(), format!("{HEAD}\n{}\n", link(L100, 0)));
}

#[test]
fn a_red_with_a_check_still_running_waits_without_rerunning() {
    let fx = Fixture::new(&["FAILURE", "IN_PROGRESS"]);
    let out = fx.ok();
    assert_eq!(first_line(&out), "PENDING");
    assert!(
        out.ends_with(
            "\nA check failed; waiting for the other checks to finish before re-running the failed jobs.\n"
        ),
        "{out}"
    );
    assert!(fx.reruns().is_empty());
    assert!(fx.record().is_none());
}

#[test]
fn a_failure_that_cannot_be_rerun_is_red_at_once() {
    let cases: [(&str, &[&str]); 3] = [
        ("non-Actions", &["https://example.com/ci/1"]),
        ("empty", &[""]),
        ("missing", &["<none>"]),
    ];
    for (what, links) in cases {
        let fx = Fixture::new(&["FAILURE"]);
        fx.set_links(links);
        assert_eq!(fx.ok(), "RED\n\nFAILURE job-0\n", "{what}");
        assert!(fx.reruns().is_empty(), "{what}");
        assert!(fx.record().is_none(), "{what}");
    }
    let fx = Fixture::new(&["FAILURE", "FAILURE"]);
    fx.set_links(&[&link(L100, 0), "https://example.com/ci/1"]);
    assert_eq!(first_line(&fx.ok()), "RED");
    assert!(fx.reruns().is_empty());
    assert!(fx.record().is_none());
}

#[test]
fn a_failing_rerun_is_red_with_a_note_and_no_record() {
    let fx = Fixture::new(&["FAILURE", "FAILURE"]);
    fx.set_links(&[&link(L100, 0), &link(L200, 1)]);
    fx.switch("fail-rerun");
    let (out, err) = out_and_err(&fx);
    assert_eq!(out, "RED\n\nFAILURE job-0\nFAILURE job-1\n");
    assert!(err.contains("choco ci-checks: "), "{err}");
    assert!(err.contains("run 100") && err.contains("exit 1"), "{err}");
    assert_eq!(
        fx.reruns(),
        vec!["run rerun 100 --failed"],
        "stops at the first failure"
    );
    assert!(fx.record().is_none());
}

#[test]
fn a_failed_record_write_is_red_with_a_note_after_the_rerun() {
    let fx = Fixture::new(&["FAILURE"]);
    fx.broken_tool("mv", 1);
    let (out, err) = out_and_err(&fx);
    assert_eq!(out, "RED\n\nFAILURE job-0\n");
    assert!(
        err.contains("choco ci-checks: re-ran the failed jobs but could not record it"),
        "{err}"
    );
    assert_eq!(fx.reruns().len(), 1);
    assert!(fx.record().is_none());
    let leftovers: Vec<_> = fs::read_dir(fx.repo.join(".git"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("choco-ci-rerun")
        })
        .collect();
    assert!(leftovers.is_empty(), "the temp file is cleaned up");
}

#[test]
fn an_unparsable_record_is_ignored_with_a_note_and_rewritten() {
    let junk = [
        String::new(),
        "garbage\n".to_string(),
        format!("{HEAD}\n"),
        format!("{HEAD}\nhttps://example.com/ci/1\n"),
    ];
    for text in junk {
        let fx = Fixture::new(&["FAILURE"]);
        fs::write(fx.record_path(), &text).unwrap();
        let (out, err) = out_and_err(&fx);
        assert_eq!(first_line(&out), "PENDING", "record {text:?}");
        assert!(
            err.contains("ignoring unparsable"),
            "record {text:?}: {err}"
        );
        assert_eq!(fx.reruns().len(), 1, "record {text:?}");
        assert_eq!(
            fx.record().unwrap(),
            format!("{HEAD}\n{}\n", link(L100, 0)),
            "record {text:?}"
        );
    }
}

#[test]
fn a_red_outside_a_repo_needs_git_only_when_it_can_be_rerun() {
    let fx = Fixture::new(&["FAILURE"]);
    let bare = fx.dir.join("not-a-repo");
    fs::create_dir_all(&bare).unwrap();
    let out = fx.run_in(&bare);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("choco ci-checks: "));
    assert!(fx.reruns().is_empty());

    fx.set_links(&["https://example.com/ci/1"]);
    let out = fx.run_in(&bare);
    assert!(out.status.success());
    assert_eq!(first_line(&String::from_utf8(out.stdout).unwrap()), "RED");
}

#[test]
fn a_red_never_prints_startup_failure() {
    let fx = Fixture::new(&["FAILURE", "STARTUP_FAILURE"]);
    assert_eq!(first_line(&fx.ok()), "PENDING");
    assert_eq!(fx.reruns().len(), 1);
    let fx = Fixture::new(&["FAILURE", "STARTUP_FAILURE"]);
    fx.set_links(&["https://example.com/ci/1", &link(L100, 1)]);
    assert_eq!(first_line(&fx.ok()), "RED");
}

#[test]
fn a_link_with_a_space_is_never_rerunnable() {
    let fx = Fixture::new(&["FAILURE"]);
    fx.set_links(&["https://github.com/o/r/actions/runs/100/job/1 x"]);
    assert_eq!(first_line(&fx.ok()), "RED");
    assert!(fx.reruns().is_empty());
}
