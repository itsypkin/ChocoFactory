//! Direct coverage for `workflows/scripts/await-merge.sh` (#379).
//!
//! The two parked human gates run this script to learn that their PR was
//! merged by hand. The workflow's watcher matches stdout whatever the exit
//! status, so every wrong answer here is silent: a `CLOSED` PR that prints
//! `MERGED`, or a failed read that leaks `MERGED` on stdout, would close a
//! task whose work never landed. This file runs the shipped executable (by
//! its shebang, as the daemon does) with a fake `gh` first on `PATH`.
//!
//! The fake `gh` records every call's arguments and applies the requested
//! `-q` filter with `jq` to one fixed JSON record, standing in for the
//! `gojq` embedded in `gh`. The shipped script itself needs no `jq`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_GH: &str = r#"#!/bin/sh
# Records one argument per line, then a `--` line, per call.
{ printf '%s\n' "$@"; echo --; } >> "$FAKE_GH_DIR/calls"
if [ -e "$FAKE_GH_DIR/fail" ]; then
  # A read that fails after printing a merged state, with its own diagnostic.
  echo MERGED
  echo "fake gh: HTTP 502 while reading the pull request" >&2
  exit 17
fi
if [ "$1" != pr ] || [ "$2" != view ] || [ "$4" != --json ] || [ "$5" != state ] || [ "$6" != -q ] || [ "$7" != .state ]; then
  echo "fake gh: unexpected call: $*" >&2
  exit 99
fi
if [ -e "$FAKE_GH_DIR/raw-output" ]; then
  # A successful call that prints exactly this, with no record behind it.
  cat "$FAKE_GH_DIR/raw-output"
  exit 0
fi
jq -r "$7" < "$FAKE_GH_DIR/record.json"
"#;

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/scripts/await-merge.sh")
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "await-merge-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        fs::write(&gh, FAKE_GH).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        Fixture { dir }
    }

    fn record(&self, json: &str) {
        let _ = fs::remove_file(self.dir.join("raw-output"));
        fs::write(self.dir.join("record.json"), json).unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
    }

    fn run(&self, pr_number: Option<&str>) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(script_path());
        cmd.env("PATH", path)
            .env("FAKE_GH_DIR", &self.dir)
            .env_remove("PR_NUMBER");
        if let Some(number) = pr_number {
            cmd.env("PR_NUMBER", number);
        }
        cmd.output().expect("failed to run await-merge.sh")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn expected_call(pr: &str) -> String {
    format!("pr\nview\n{pr}\n--json\nstate\n-q\n.state\n--\n")
}

#[test]
fn the_script_is_executable_with_a_posix_sh_shebang() {
    let mode = fs::metadata(script_path()).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "await-merge.sh must be executable");
    let text = fs::read_to_string(script_path()).unwrap();
    assert!(text.starts_with("#!/bin/sh\n"));
    assert!(!text.contains("jq"), "the script must not need jq");
}

#[test]
fn merge_probe_only_accepts_successful_merged_state() {
    let fixture = Fixture::new("states");
    // A mixed sequence: nothing but the exact MERGED record prints, and a
    // merged one in the middle is not remembered by the next read.
    let records: &[(&str, &str)] = &[
        (r#"{"state":"OPEN"}"#, ""),
        (r#"{"state":"MERGED"}"#, "MERGED\n"),
        (r#"{"state":"CLOSED"}"#, ""),
        (r#"{"state":"MERGED"}"#, "MERGED\n"),
        (r#"{"state":"UNKNOWN"}"#, ""),
        (r#"{"state":"merged"}"#, ""),
        (r#"{"state":"MERGED "}"#, ""),
        (r#"{"state":"MERGED\nOPEN"}"#, ""),
        (r#"{"state":""}"#, ""),
        (r#"{}"#, ""),
        (r#"{"state":null}"#, ""),
        (r#"{"state":true}"#, ""),
        (r#"{"state":7}"#, ""),
        (r#"{"state":{"name":"MERGED"}}"#, ""),
        (r#"{"state":["MERGED"]}"#, ""),
        (r#"{"state":"OPEN","mergedAt":"2026-11-14T14:43:38Z"}"#, ""),
        (r#"{"state":"CLOSED","mergedAt":null}"#, ""),
    ];
    let mut expected_calls = String::new();
    for (record, expected_stdout) in records {
        fixture.record(record);
        let out = fixture.run(Some("387"));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            *expected_stdout,
            "record {record}"
        );
        assert!(out.status.success(), "record {record}: {:?}", out.status);
        expected_calls.push_str(&expected_call("387"));
        assert_eq!(fixture.calls(), expected_calls, "record {record}");
    }
}

#[test]
fn an_empty_successful_read_is_not_merged() {
    let fixture = Fixture::new("empty-output");
    fs::write(fixture.dir.join("raw-output"), "").unwrap();
    let out = fixture.run(Some("388"));
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(fixture.calls(), expected_call("388"));
}

#[test]
fn a_failed_read_keeps_its_status_and_diagnostic_and_prints_no_merge() {
    let fixture = Fixture::new("failed");
    fs::write(fixture.dir.join("fail"), "").unwrap();
    let out = fixture.run(Some("389"));
    assert_eq!(out.status.code(), Some(17));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "fake gh: HTTP 502 while reading the pull request\n"
    );
    assert_eq!(fixture.calls(), expected_call("389"));
}

#[test]
fn no_pr_number_means_no_gh_call_and_no_output_even_if_gh_would_say_merged() {
    let fixture = Fixture::new("no-pr");
    fixture.record(r#"{"state":"MERGED"}"#);
    for pr_number in [None, Some("")] {
        let out = fixture.run(pr_number);
        assert!(out.status.success(), "{pr_number:?}: {:?}", out.status);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{pr_number:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{pr_number:?}");
        assert_eq!(fixture.calls(), "", "{pr_number:?} must not call gh");
    }
    // The same fixture does answer MERGED once there is a number.
    let out = fixture.run(Some("390"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "MERGED\n");
}

#[test]
fn the_script_reads_no_comments_reviews_or_conflicts() {
    // A PR whose record carries review and conflict data and an unmerged
    // state: the only call made is the one state read.
    let fixture = Fixture::new("no-review-reads");
    fixture.record(
        r#"{"state":"OPEN","mergeable":"CONFLICTING","reviews":[{"state":"APPROVED"},{"state":"CHANGES_REQUESTED"}],"comments":[{"body":"/approve"},{"body":"/request-changes"}]}"#,
    );
    let out = fixture.run(Some("391"));
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(fixture.calls(), expected_call("391"));
}
