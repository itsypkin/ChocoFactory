//! Direct coverage for `coding-task.yaml`'s verdict filter (#78).
//!
//! `awaiting_human_review` decides the whole workflow's terminal edge from
//! a jq filter embedded in its `command:`. The workflow tests in
//! `engine.rs`/`e2e_smoke.rs` stub `gh` wholesale, so they exercise the
//! routing on either side of that filter and none of the filter itself —
//! and the filter is where the interesting failure modes live, because
//! every one of them silently returns the *wrong* verdict rather than an
//! error. This file crosses that seam.
//!
//! The filter is read out of the real workflow file rather than copied, so
//! these cases cannot drift away from what actually ships. It is run with
//! `jq`, which stands in for the `gojq` embedded in `gh --jq`; the subset
//! used here (`IN`, `split`, `sub`, `index`, `$ENV`, `//`) behaves
//! identically in both.

use std::io::Write;
use std::process::{Command, Stdio};

/// The single-quoted jq program out of `awaiting_human_review`'s folded
/// `command:`, with the YAML fold applied.
///
/// Deliberately parsed rather than duplicated: a copy would keep passing
/// after someone edited the workflow, which is the one thing these tests
/// exist to prevent.
fn verdict_filter() -> String {
    let yaml = include_str!("../../workflows/coding-task.yaml");
    let stage = yaml
        .split_once("  awaiting_human_review:")
        .expect("awaiting_human_review stage missing from coding-task.yaml")
        .1;
    let body = stage
        .split_once("    interval:")
        .expect("stage has no interval: key")
        .0;
    let folded = body.lines().map(str::trim).collect::<Vec<_>>().join(" ");
    let after = folded
        .split_once('\'')
        .expect("no single-quoted jq filter in the command")
        .1;
    after
        .rsplit_once('\'')
        .expect("unterminated jq filter")
        .0
        .to_string()
}

/// Runs the shipped filter over `comments`, with `SINCE` as the head
/// commit's timestamp, and returns what the stage would see on stdout.
///
/// `tail -n 1` mirrors the command's own final pipe: the filter emits one
/// token per qualifying comment, oldest first, and the newest wins.
fn verdict(since: &str, comments: &str) -> String {
    let mut child = Command::new("jq")
        .args(["-r", &verdict_filter()])
        .env("SINCE", since)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jq not found on PATH — it is required to run this suite");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(comments.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "filter errored: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .last()
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
        r#"{{"created_at": "{at}", "updated_at": "{at}", "author_association": "{assoc}", "user": {user}, "body": {}}}"#,
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
        r#"{{"created_at": "{STALE}", "updated_at": "{FRESH}", "author_association": "OWNER", "user": {{"login": "me"}}, "body": "/approve"}}"#
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
