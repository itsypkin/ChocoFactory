//! Direct coverage for `workflows/scripts/await-review.sh` (#78, #138).
//!
//! `awaiting_human_review` decides the whole workflow's terminal edge from
//! that script, and hands the human's comments to the coder. The workflow
//! tests in `engine.rs`/`e2e_smoke.rs` stub `gh` wholesale, so they cover the
//! routing either side of it and none of the script itself, which is where
//! the interesting failure modes live: every one silently returns the
//! *wrong* verdict rather than an error. This file crosses that seam by
//! running the shipped script with a fake `gh` first on `PATH`.
//!
//! The fake `gh` applies `-q` by piping its canned pages through `jq`,
//! which stands in for the `gojq` embedded in `gh --jq`; the subset used
//! (`IN`, `split`, `sub`, `index`, `$ENV`, `//`, string interpolation)
//! behaves identically. The shipped script itself needs no `jq`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_GH: &str = r#"#!/bin/sh
# Pages are $DIR/page-1.json, page-2.json ... (a `gh api` comments call
# prints each page's `-q` output in turn only with --paginate). $DIR/since is the
# head commit's date. $DIR/fail-<what> makes that call fail, where <what> is
# pr-view, commits, or comments-<n> for the nth comments call.
DIR="$GH_FAKE_DIR"
q=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-q" ]; then q=$a; fi
    prev=$a
done
case "$1" in
pr)
    [ -e "$DIR/fail-pr-view" ] && { echo "fake gh: pr view failed" >&2; exit 1; }
    [ -e "$DIR/fail-empty-pr-view" ] && exit 0
    echo "0123456789abcdef"
    ;;
api)
    case "$2$3" in
    *commits/*)
        [ -e "$DIR/fail-commits" ] && { echo "fake gh: commits failed" >&2; exit 1; }
        [ -e "$DIR/fail-empty-commits" ] && exit 0
        cat "$DIR/since"
        ;;
    *)
        n=$(cat "$DIR/count" 2>/dev/null || echo 0)
        n=$((n+1))
        echo "$n" > "$DIR/count"
        [ -e "$DIR/fail-comments-$n" ] && { echo "fake gh: comments failed" >&2; exit 1; }
        # Like gh: only the first page unless --paginate is given.
        paginate=0
        for a in "$@"; do [ "$a" = "--paginate" ] && paginate=1; done
        for page in "$DIR"/page-*.json; do
            jq -r "$q" < "$page" || exit 1
            [ "$paginate" = 1 ] || break
        done
        ;;
    esac
    ;;
*) echo "fake gh: unhandled: $*" >&2; exit 1 ;;
esac
"#;

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/scripts/await-review.sh")
}

/// One fake-`gh` directory: the pages, the head date and failure switches.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(since: &str, pages: &[String]) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "await-review-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        fs::write(&gh, FAKE_GH).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("since"), since).unwrap();
        for (i, page) in pages.iter().enumerate() {
            fs::write(dir.join(format!("page-{}.json", i + 1)), page).unwrap();
        }
        Fixture { dir }
    }

    fn fail(&self, what: &str) {
        fs::write(self.dir.join(format!("fail-{what}")), "").unwrap();
    }

    fn run(&self) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg(script_path())
            .env("PATH", path)
            .env("GH_FAKE_DIR", &self.dir)
            .env("PR_NUMBER", "7")
            .output()
            .expect("failed to run await-review.sh")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout_of(since: &str, pages: &[String]) -> String {
    let fx = Fixture::new(since, pages);
    let out = fx.run();
    assert!(
        out.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The verdict token, which is line 1 of the output (empty when none).
fn verdict(since: &str, comments: &str) -> String {
    stdout_of(since, &[comments.to_string()])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

const SINCE: &str = "2026-01-02T00:00:00Z";
const FRESH: &str = "2026-01-03T00:00:00Z";
const STALE: &str = "2026-01-01T00:00:00Z";

/// One comment, JSON-encoded. `body` is passed through `serde_json` so a
/// case can contain newlines, quotes and CRs without hand-escaping.
fn comment(at: &str, assoc: &str, login: Option<&str>, body: &str) -> String {
    let user = match login {
        Some(login) => format!(r#"{{"login": {}}}"#, serde_json::json!(login)),
        None => "null".to_string(),
    };
    format!(
        r#"{{"created_at": "{at}", "updated_at": "{at}", "author_association": "{assoc}", "user": {user}, "html_url": "https://example.test/c/{at}", "body": {}}}"#,
        serde_json::json!(body)
    )
}

fn list(comments: &[String]) -> String {
    format!("[{}]", comments.join(","))
}

#[test]
fn a_fresh_owner_marker_is_the_verdict() {
    let body = "Two findings, one worth fixing before merge.\n\n/request-changes\n";
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(FRESH, "OWNER", Some("maintainer"), body)])
        ),
        "REQUEST_CHANGES"
    );
}

#[test]
fn approve_and_request_changes_are_distinguished() {
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(FRESH, "OWNER", Some("me"), "ship it\n\n/approve\n")])
        ),
        "APPROVE"
    );
}

/// The freshness bound, which is what stops a verdict the coder already
/// acted on from being re-read on the next lap.
#[test]
fn a_marker_older_than_the_head_commit_is_ignored() {
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(STALE, "OWNER", Some("me"), "/approve")])
        ),
        ""
    );
}

/// Editing an existing comment to add the marker has to count: it is the
/// first move a reviewer who already wrote their prose reaches for. This
/// is why the bound is `max(created_at, updated_at)` and not `created_at`.
#[test]
fn a_stale_comment_edited_after_the_head_commit_counts() {
    let edited = format!(
        r#"{{"created_at": "{STALE}", "updated_at": "{FRESH}", "author_association": "OWNER", "user": {{"login": "me"}}, "html_url": "https://example.test/c/e", "body": "/approve"}}"#
    );
    assert_eq!(verdict(SINCE, &list(&[edited])), "APPROVE");
}

/// The repo is public, so this fence is the whole authorization story.
#[test]
fn an_outsiders_marker_is_not_a_verdict() {
    for assoc in ["NONE", "CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR"] {
        assert_eq!(
            verdict(
                SINCE,
                &list(&[comment(FRESH, assoc, Some("passer-by"), "/approve")])
            ),
            "",
            "{assoc} must not be able to vote"
        );
    }
}

#[test]
fn a_bot_marker_is_not_a_verdict() {
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(
                FRESH,
                "COLLABORATOR",
                Some("github-actions[bot]"),
                "/approve"
            )])
        ),
        ""
    );
}

/// A deleted account leaves `user: null`. Before the `// ""` guard this
/// threw inside jq, which the poll would have seen as empty output —
/// indistinguishable from "nobody has reviewed yet", for six hours.
#[test]
fn a_null_author_does_not_error_the_filter() {
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(FRESH, "OWNER", None, "/request-changes")])
        ),
        "REQUEST_CHANGES"
    );
}

/// Precedence: a body carrying both markers must never resolve to
/// "merge it".
#[test]
fn a_comment_with_both_markers_requests_changes() {
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(
                FRESH,
                "OWNER",
                Some("me"),
                "/approve\n/request-changes\n"
            )])
        ),
        "REQUEST_CHANGES"
    );
}

/// The marker is matched as a whole line, so GitHub's quote-reply prefix
/// and an inline mention both fail to vote.
#[test]
fn a_quoted_or_inline_marker_is_not_a_verdict() {
    for body in [
        "> /approve\n",
        "use /approve to vote",
        "  /approve",
        "/approved",
        "/approve now",
    ] {
        assert_eq!(
            verdict(SINCE, &list(&[comment(FRESH, "OWNER", Some("me"), body)])),
            "",
            "{body:?} must not vote"
        );
    }
}

/// A comment typed in the browser arrives CRLF-terminated, and reviewers
/// leave trailing spaces.
#[test]
fn trailing_whitespace_and_carriage_returns_still_vote() {
    for body in ["/approve  \r\n", "/approve\r\n", "/approve   "] {
        assert_eq!(
            verdict(SINCE, &list(&[comment(FRESH, "OWNER", Some("me"), body)])),
            "APPROVE",
            "{body:?} should vote"
        );
    }
}

/// The newest qualifying verdict wins, not the first one found — a
/// reviewer who changes their mind in a later comment must be obeyed.
#[test]
fn the_newest_qualifying_verdict_wins() {
    let older = comment(FRESH, "OWNER", Some("me"), "/request-changes");
    let newer = comment("2026-01-04T00:00:00Z", "OWNER", Some("me"), "/approve");
    assert_eq!(verdict(SINCE, &list(&[older, newer])), "APPROVE");
}

/// Ordinary prose after a verdict does not undo it.
///
/// This is a real semantics change and worth pinning deliberately rather
/// than leaving incidental. The filter emits one token per *marker-bearing*
/// comment and the newest wins, so a later comment with no marker is
/// invisible — where an earlier design that returned the newest comment's
/// whole body would have let "wait, hold off" mask the approval. Silently
/// vetoing a verdict with prose is its own trap; the only retraction is
/// the other marker.
#[test]
fn a_prose_comment_does_not_retract_an_earlier_verdict() {
    let verdict_comment = comment(FRESH, "OWNER", Some("me"), "/approve");
    let second_thoughts = comment(
        "2026-01-04T00:00:00Z",
        "OWNER",
        Some("me"),
        "wait, hold off — I want another look",
    );
    assert_eq!(
        verdict(SINCE, &list(&[verdict_comment, second_thoughts])),
        "APPROVE"
    );
}

/// …and the documented way to actually change your mind does work.
#[test]
fn the_other_marker_retracts_an_earlier_verdict() {
    let approved = comment(FRESH, "OWNER", Some("me"), "/approve");
    let retracted = comment(
        "2026-01-04T00:00:00Z",
        "OWNER",
        Some("me"),
        "on reflection:\n\n/request-changes",
    );
    assert_eq!(
        verdict(SINCE, &list(&[approved, retracted])),
        "REQUEST_CHANGES"
    );
}

#[test]
fn no_comments_at_all_yields_no_verdict() {
    assert_eq!(verdict(SINCE, "[]"), "");
}

/// A comment with no marker leaves the stage polling rather than
/// resolving it either way — the ordinary case while a review is being
/// written.
#[test]
fn prose_without_a_marker_yields_no_verdict() {
    assert_eq!(
        verdict(
            SINCE,
            &list(&[comment(
                FRESH,
                "OWNER",
                Some("me"),
                "Looking at this now, back shortly."
            )])
        ),
        ""
    );
}

// ---- #138: rendering, failures, pagination, outcome patterns ----

const LATER: &str = "2026-01-04T00:00:00Z";

/// Excluded comments appear neither in the verdict nor the rendered list.
#[test]
fn excluded_comments_are_neither_voted_nor_rendered() {
    let mut cs = vec![comment(
        FRESH,
        "OWNER",
        Some("owner"),
        "OWNERBODY\n/approve",
    )];
    for (i, assoc) in ["CONTRIBUTOR", "NONE", "FIRST_TIME_CONTRIBUTOR"]
        .iter()
        .enumerate()
    {
        cs.push(comment(
            FRESH,
            assoc,
            Some("outsider"),
            &format!("OUTSIDER{i}"),
        ));
    }
    cs.push(comment(FRESH, "COLLABORATOR", Some("ci[bot]"), "BOTBODY"));
    cs.push(comment(STALE, "OWNER", Some("owner"), "OLDBODY"));
    let out = stdout_of(SINCE, &[list(&cs)]);
    assert!(out.starts_with("APPROVE\n\n"), "{out}");
    assert!(out.contains("OWNERBODY"));
    for gone in ["OUTSIDER", "BOTBODY", "OLDBODY"] {
        assert!(!out.contains(gone), "{gone} leaked:\n{out}");
    }
}

#[test]
fn an_edited_comment_counts_and_renders_with_edited() {
    let edited = format!(
        r#"{{"created_at": "{STALE}", "updated_at": "{FRESH}", "author_association": "OWNER", "user": {{"login": "me"}}, "html_url": "https://example.test/c/e", "body": "findings\n/request-changes"}}"#
    );
    let out = stdout_of(SINCE, &[list(&[edited])]);
    assert_eq!(
        out,
        format!(
            "REQUEST_CHANGES\n\n### me (OWNER), {STALE}, edited {FRESH}\n\
             https://example.test/c/e\n\nfindings\n/request-changes\n\n"
        )
    );
}

#[test]
fn an_unedited_comment_renders_without_edited() {
    let out = stdout_of(
        SINCE,
        &[list(&[comment(FRESH, "MEMBER", Some("me"), "/approve")])],
    );
    assert_eq!(
        out,
        format!(
            "APPROVE\n\n### me (MEMBER), {FRESH}\nhttps://example.test/c/{FRESH}\n\n/approve\n\n"
        )
    );
}

/// Findings in one comment and the marker in another: both are handed
/// over, oldest first.
#[test]
fn a_comment_without_a_marker_is_rendered_alongside_the_marker_one() {
    let findings = comment(FRESH, "OWNER", Some("me"), "FINDINGS here");
    let marker = comment(LATER, "OWNER", Some("me"), "/request-changes");
    let out = stdout_of(SINCE, &[list(&[findings, marker])]);
    assert!(
        out.starts_with("REQUEST_CHANGES\n\n### me (OWNER)"),
        "{out}"
    );
    let a = out.find("FINDINGS here").unwrap();
    let b = out.find("/request-changes\n").unwrap();
    assert!(a < b, "oldest first:\n{out}");
    assert_eq!(out.matches("### me").count(), 2);
}

#[test]
fn no_marker_means_empty_stdout_even_with_qualifying_comments() {
    let out = stdout_of(
        SINCE,
        &[list(&[comment(FRESH, "OWNER", Some("me"), "just prose")])],
    );
    assert_eq!(out, "");
}

#[test]
fn a_gh_failure_in_any_call_is_an_error_with_empty_stdout() {
    let page = list(&[comment(FRESH, "OWNER", Some("me"), "/approve")]);
    for what in ["pr-view", "commits", "comments-1", "comments-2"] {
        let fx = Fixture::new(SINCE, std::slice::from_ref(&page));
        fx.fail(what);
        let out = fx.run();
        assert!(!out.status.success(), "{what} must fail the script");
        assert!(out.stdout.is_empty(), "{what}: no partial verdict");
        assert!(!out.stderr.is_empty(), "{what}: stderr must say why");
    }
}

/// Newest marker across pages decides, and every page's comments render.
#[test]
fn comments_spread_over_two_pages_are_all_rendered_and_the_newest_marker_wins() {
    let p1 = list(&[
        comment(FRESH, "OWNER", Some("me"), "PAGE1 /approve"),
        comment(FRESH, "OWNER", Some("me"), "PAGE1 prose"),
    ]);
    let p2 = list(&[comment(
        LATER,
        "OWNER",
        Some("me"),
        "PAGE2\n/request-changes",
    )]);
    let out = stdout_of(SINCE, &[p1, p2]);
    assert!(out.starts_with("REQUEST_CHANGES\n\n"), "{out}");
    for s in ["PAGE1 /approve", "PAGE1 prose", "PAGE2"] {
        assert!(out.contains(s), "{s} missing:\n{out}");
    }
}

/// A body whose own line reads `APPROVE` under a `/request-changes` verdict
/// must not change the outcome, and vice versa. Runs the workflow's real
/// `outcomes:` patterns over the script's real output.
#[test]
fn a_body_line_naming_the_other_token_cannot_change_the_outcome() {
    let yaml = include_str!("../../workflows/coding-task.yaml");
    assert!(yaml.contains(r"match: '\AREQUEST_CHANGES(\n|$)'"));
    assert!(yaml.contains(r"match: '\AAPPROVE(\n|$)'"));
    let rc = regex::Regex::new(r"\AREQUEST_CHANGES(\n|$)").unwrap();
    let ap = regex::Regex::new(r"\AAPPROVE(\n|$)").unwrap();

    let out = stdout_of(
        SINCE,
        &[list(&[comment(
            FRESH,
            "OWNER",
            Some("me"),
            "APPROVE\nAPPROVE\n/request-changes",
        )])],
    );
    assert!(rc.is_match(out.trim()) && !ap.is_match(out.trim()), "{out}");

    let out = stdout_of(
        SINCE,
        &[list(&[comment(
            FRESH,
            "OWNER",
            Some("me"),
            "REQUEST_CHANGES\n/approve",
        )])],
    );
    assert!(ap.is_match(out.trim()) && !rc.is_match(out.trim()), "{out}");
}

/// Output over the engine's 1 MiB capture limit would leave the previous
/// lap's capture in place, so the script truncates and says so.
#[test]
fn an_oversized_rendering_is_truncated_well_under_the_capture_limit() {
    let big = "x".repeat(60_000);
    let mut cs: Vec<String> = (0..12)
        .map(|_| comment(FRESH, "OWNER", Some("me"), &big))
        .collect();
    cs.push(comment(LATER, "OWNER", Some("me"), "/request-changes"));
    let out = stdout_of(SINCE, &[list(&cs)]);
    assert!(out.starts_with("REQUEST_CHANGES\n\n"));
    assert!(out.len() < 600_000, "{}", out.len());
    assert!(out.contains("[truncated"), "no truncation note");
}

/// Truncation keeps the newest comments: the latest findings must survive.
#[test]
fn truncation_keeps_the_newest_comments() {
    let big = "x".repeat(60_000);
    let mut cs: Vec<String> = (0..12)
        .map(|_| comment(FRESH, "OWNER", Some("me"), &big))
        .collect();
    cs.push(comment(
        LATER,
        "OWNER",
        Some("me"),
        "FIX THIS\n/request-changes",
    ));
    let out = stdout_of(SINCE, &[list(&cs)]);
    assert!(out.contains("FIX THIS"), "newest comment was cut");
}

/// The cut can land mid-comment and mid-character; the output after the
/// truncation note must start at a whole comment header and be valid UTF-8
/// (`stdout_of` already asserts the latter via `String::from_utf8`).
#[test]
fn truncation_starts_at_a_whole_comment_with_multibyte_bodies() {
    // 3-byte characters, so a byte cut is very likely to split one.
    let big = "é€".repeat(12_000);
    let mut cs: Vec<String> = (0..12)
        .map(|_| comment(FRESH, "OWNER", Some("me"), &big))
        .collect();
    cs.push(comment(LATER, "OWNER", Some("me"), "/request-changes"));
    let out = stdout_of(SINCE, &[list(&cs)]);
    let (_, after) = out.split_once("]\n\n").expect("no truncation note");
    assert!(out.contains("[truncated"), "no truncation note");
    assert!(after.starts_with("### me (OWNER), "), "{:?}", &after[..80]);
}

/// Empty gh answers and a missing PR_NUMBER are errors, not "no verdict".
#[test]
fn empty_head_empty_date_and_missing_pr_number_are_errors() {
    let page = list(&[comment(FRESH, "OWNER", Some("me"), "/approve")]);
    for what in ["empty-pr-view", "empty-commits"] {
        let fx = Fixture::new(SINCE, std::slice::from_ref(&page));
        fx.fail(what);
        let out = fx.run();
        assert!(!out.status.success(), "{what} must fail the script");
        assert!(out.stdout.is_empty(), "{what}: no partial verdict");
        assert!(!out.stderr.is_empty(), "{what}: stderr must say why");
    }
    let fx = Fixture::new(SINCE, std::slice::from_ref(&page));
    let path = format!("{}:{}", fx.dir.display(), std::env::var("PATH").unwrap());
    let out = Command::new("sh")
        .arg(script_path())
        .env("PATH", path)
        .env("GH_FAKE_DIR", &fx.dir)
        .env_remove("PR_NUMBER")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}
