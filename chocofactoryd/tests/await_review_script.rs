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
# Issue-comment pages are $DIR/page-1.json, page-2.json ... (a `gh api` call
# prints each page's `-q` output in turn only with --paginate); reviews are
# reviews-page-N.json and inline review comments review-comments-page-N.json.
# $DIR/since is the head commit's date. $DIR/fail-<what> makes that call
# fail, where <what> is pr-view, commits, or comments-<n>, reviews-<n> or
# review-comments-<n> for the nth call to that endpoint.
# $DIR/fail-<pages prefix>-<i>.json (e.g. fail-reviews-page-2.json) makes page
# i of that endpoint fail under --paginate, after pages 1..i-1 were printed.
DIR="$GH_FAKE_DIR"
echo "$*" >> "$DIR/calls"
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
    # $DIR/pr-state is OPEN (default), CLOSED or MERGED; a merged PR carries
    # a mergedAt. The `-q` filter is applied like gh does.
    state=$(cat "$DIR/pr-state" 2>/dev/null || echo OPEN)
    merged=null
    [ "$state" = MERGED ] && [ ! -e "$DIR/no-merged-at" ] && merged='"2030-01-02T03:04:05Z"'
    printf '{"headRefOid":"0123456789abcdef","state":"%s","mergedAt":%s}' "$state" "$merged" | jq -r "$q"
    ;;
api)
    # pages <file prefix> <counter file> <fail switch prefix>: the nth call to
    # this endpoint fails when $DIR/<switch>-<n> exists; pages are
    # $DIR/<prefix>-N.json, none meaning an empty list.
    paginate=0
    for a in "$@"; do [ "$a" = "--paginate" ] && paginate=1; done
    pages() {
        n=$(cat "$DIR/$2" 2>/dev/null || echo 0)
        n=$((n+1))
        echo "$n" > "$DIR/$2"
        [ -e "$DIR/$3-$n" ] && { echo "fake gh: $3 failed" >&2; exit 1; }
        # empty-<switch>-<n>: succeed with nothing (the item went away).
        [ -e "$DIR/empty-$3-$n" ] && exit 0
        # Like gh: only the first page unless --paginate is given.
        found=0
        i=0
        for page in "$DIR"/$1-*.json; do
            [ -e "$page" ] || continue
            found=1
            i=$((i+1))
            [ -e "$DIR/fail-$1-$i.json" ] && { echo "fake gh: $1 page $i failed" >&2; exit 1; }
            jq -r "$q" < "$page" || exit 1
            [ "$paginate" = 1 ] || break
        done
        [ "$found" = 1 ] || echo '[]' | jq -r "$q"
    }
    case "$2$3" in
    *commits/*)
        [ -e "$DIR/fail-commits" ] && { echo "fake gh: commits failed" >&2; exit 1; }
        [ -e "$DIR/fail-empty-commits" ] && exit 0
        cat "$DIR/since"
        ;;
    */issues/*/comments*) pages page count fail-comments ;;
    */pulls/*/reviews*) pages reviews-page reviews-count fail-reviews ;;
    */pulls/*/comments*) pages review-comments-page review-comments-count fail-review-comments ;;
    *) echo "fake gh: unhandled: $*" >&2; exit 1 ;;
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

    /// Reviews pages, as `reviews-page-N.json`.
    fn reviews(&self, pages: &[String]) {
        for (i, page) in pages.iter().enumerate() {
            fs::write(self.dir.join(format!("reviews-page-{}.json", i + 1)), page).unwrap();
        }
    }

    /// Inline review-comment pages, as `review-comments-page-N.json`.
    fn review_comments(&self, pages: &[String]) {
        for (i, page) in pages.iter().enumerate() {
            fs::write(
                self.dir
                    .join(format!("review-comments-page-{}.json", i + 1)),
                page,
            )
            .unwrap();
        }
    }

    /// The PR's state as `gh pr view` reports it: OPEN, CLOSED or MERGED.
    fn pr_state(&self, state: &str) {
        fs::write(self.dir.join("pr-state"), state).unwrap();
    }

    /// Every argument line the fake `gh` was called with.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
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

/// A failure while building a truncated capture must not leave a verdict on
/// stdout: the poll matches outcomes on stdout whatever the exit code.
#[test]
fn a_failure_mid_truncation_prints_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let big = "x".repeat(60_000);
    let mut cs: Vec<String> = (0..12)
        .map(|_| comment(FRESH, "OWNER", Some("me"), &big))
        .collect();
    cs.push(comment(LATER, "OWNER", Some("me"), "/request-changes"));
    let fx = Fixture::new(SINCE, &[list(&cs)]);
    let sed = fx.dir.join("sed");
    fs::write(&sed, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&sed, fs::Permissions::from_mode(0o755)).unwrap();
    let out = fx.run();
    assert!(!out.status.success(), "a failing sed must fail the script");
    assert!(out.stdout.is_empty(), "no partial verdict on stdout");
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
    // 2- and 3-byte characters, so a byte cut is very likely to split one.
    let big = "é€".repeat(12_000);
    let mut cs: Vec<String> = (0..12)
        .map(|_| comment(FRESH, "OWNER", Some("me"), &big))
        .collect();
    cs.push(comment(LATER, "OWNER", Some("me"), "/request-changes"));
    let out = stdout_of(SINCE, &[list(&cs)]);
    let (_, after) = out.split_once("]\n\n").expect("no truncation note");
    assert!(out.contains("[truncated"), "no truncation note");
    assert!(
        after.starts_with("### me (OWNER), "),
        "{:?}",
        after.chars().take(80).collect::<String>()
    );
}

/// A single comment over the cap leaves no header in the kept tail; its end
/// (the findings and the marker) must still come through.
#[test]
fn truncation_of_one_oversized_comment_keeps_its_end() {
    let body = format!("{}\nFIX THIS\n/request-changes", "x".repeat(600_000));
    let cs = vec![comment(FRESH, "OWNER", Some("me"), &body)];
    let out = stdout_of(SINCE, &[list(&cs)]);
    assert!(out.contains("[truncated"), "no truncation note");
    assert!(out.contains("FIX THIS"), "newest text was dropped");
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

// ---- a merged PR counts as approval (#102) ----

fn merged_fixture(comments: &str) -> Fixture {
    let fx = Fixture::new(SINCE, &[comments.to_string()]);
    fx.pr_state("MERGED");
    fx
}

/// MERGED, a blank line, the merged-at line, exit 0, and the comments are
/// never read.
#[test]
fn a_merged_pr_prints_merged_and_never_reads_the_comments() {
    let fx = merged_fixture(&list(&[]));
    let out = fx.run();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "MERGED\n\nThe PR was merged at 2030-01-02T03:04:05Z.\n"
    );
    let calls = fx.calls();
    assert!(!calls.contains("comments"), "comments were read: {calls}");
    assert!(
        !calls.contains("commits/"),
        "nothing else is needed: {calls}"
    );
}

/// Merged wins: once the work has landed there is nothing to revise.
#[test]
fn a_merged_pr_wins_over_a_fresh_request_changes() {
    let fx = merged_fixture(&list(&[comment(
        FRESH,
        "OWNER",
        Some("me"),
        "/request-changes",
    )]));
    let out = fx.run();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().next(), Some("MERGED"));
    assert!(!stdout.contains("REQUEST_CHANGES"));
}

/// Closed without merging is nothing new: no marker, no verdict; a marker
/// still counts exactly as on an open PR.
#[test]
fn a_closed_unmerged_pr_polls_on_as_before() {
    let none = Fixture::new(SINCE, &[list(&[])]);
    none.pr_state("CLOSED");
    let out = none.run();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());

    let approved = Fixture::new(
        SINCE,
        &[list(&[comment(FRESH, "OWNER", Some("me"), "/approve")])],
    );
    approved.pr_state("CLOSED");
    let out = approved.run();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().next(), Some("APPROVE"));
}

/// The existing rule on the new call: a `gh pr view` failure is stderr, a
/// non-zero exit and nothing on stdout.
#[test]
fn a_failing_pr_view_fails_the_script_with_empty_stdout() {
    let fx = Fixture::new(
        SINCE,
        &[list(&[comment(FRESH, "OWNER", Some("me"), "/approve")])],
    );
    fx.fail("pr-view");
    let out = fx.run();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}

/// The two new guards: `gh pr view` answering without a state, or a merged
/// PR without a `mergedAt`, is a failure (stderr, non-zero, empty stdout),
/// never a guess.
#[test]
fn an_empty_pr_state_fails_the_script_with_empty_stdout() {
    let fx = Fixture::new(SINCE, &[list(&[])]);
    fx.pr_state("");
    let out = fx.run();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}

#[test]
fn a_merged_pr_without_merged_at_fails_the_script_with_empty_stdout() {
    let fx = Fixture::new(SINCE, &[list(&[])]);
    fx.pr_state("MERGED");
    fx.fail("no-merged-at");
    // `fail` writes `fail-<what>`; the fake looks for `no-merged-at`.
    fs::rename(
        fx.dir.join("fail-no-merged-at"),
        fx.dir.join("no-merged-at"),
    )
    .unwrap();
    let out = fx.run();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}

/// The case table shared with `reply_verdict` (`engine/gate.rs`): one line
/// rule, two implementations, one table (#175).
#[derive(serde::Deserialize)]
struct MarkerCase {
    name: String,
    body: String,
    github: String,
    choco: String,
    #[serde(default)]
    note: Option<String>,
}

fn marker_cases() -> Vec<MarkerCase> {
    serde_json::from_str(include_str!("fixtures/review-markers.json")).unwrap()
}

#[test]
fn the_script_agrees_with_the_shared_marker_case_table() {
    for case in marker_cases() {
        let got = verdict(
            SINCE,
            &list(&[comment(FRESH, "OWNER", Some("owner"), &case.body)]),
        );
        assert_eq!(
            got, case.github,
            "case '{}': body {:?}",
            case.name, case.body
        );
    }
}

#[test]
fn the_marker_case_table_is_consistent() {
    let cases = marker_cases();
    for case in &cases {
        match case.choco.as_str() {
            "refused_no_marker" => assert_eq!(case.github, "", "case '{}'", case.name),
            "approved" => assert_eq!(case.github, "APPROVE", "case '{}'", case.name),
            "changes_requested" => {
                assert_eq!(case.github, "REQUEST_CHANGES", "case '{}'", case.name)
            }
            "refused_conflict" => assert!(
                case.note.is_some(),
                "case '{}': a conflict needs a note",
                case.name
            ),
            other => panic!("case '{}': unknown choco result '{other}'", case.name),
        }
        if case.note.is_some() {
            assert_eq!(case.choco, "refused_conflict", "case '{}'", case.name);
        }
    }
    assert_eq!(cases.iter().filter(|c| c.note.is_some()).count(), 1);
    for name in [
        "bare approve",
        "bare request",
        "prose then marker",
        "marker then prose",
        "trailing whitespace",
        "CRLF",
        "quoted",
        "inline",
        "indented",
        "typo",
        "wrong case",
        "no marker",
        "empty",
        "same marker twice",
        "both markers",
        "marker in a code fence",
    ] {
        assert!(
            cases.iter().any(|c| c.name == name),
            "missing case '{name}'"
        );
    }
}

// ---- GitHub reviews and inline review comments (#230) ----

/// One review, JSON-encoded. `id` is a string so a case can pass a
/// non-numeric one; `submitted_at` is `None` for a pending review.
fn review(id: &str, state: &str, at: Option<&str>, assoc: &str, login: &str, body: &str) -> String {
    format!(
        r#"{{"id": {}, "state": "{state}", "submitted_at": {}, "author_association": "{assoc}", "user": {{"login": {}}}, "html_url": "https://example.test/r/{id}", "body": {}}}"#,
        id.parse::<u64>()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| serde_json::json!(id).to_string()),
        serde_json::json!(at),
        serde_json::json!(login),
        serde_json::json!(body)
    )
}

/// A qualifying-by-default review: COLLABORATOR "rev", id 11.
fn rev(state: &str, at: &str, body: &str) -> String {
    review("11", state, Some(at), "COLLABORATOR", "rev", body)
}

struct Inline<'a> {
    review_id: u64,
    path: &'a str,
    line: Option<u64>,
    original_line: Option<u64>,
    start_line: Option<u64>,
    original_start_line: Option<u64>,
    subject: &'a str,
    at: &'a str,
    body: &'a str,
    login: Option<&'a str>,
    assoc: Option<&'a str>,
}

impl<'a> Inline<'a> {
    fn at_line(review_id: u64, path: &'a str, line: u64, body: &'a str) -> Self {
        Inline {
            review_id,
            path,
            line: Some(line),
            original_line: Some(line),
            start_line: None,
            original_start_line: None,
            subject: "line",
            at: FRESH,
            body,
            login: Some("rev"),
            assoc: Some("COLLABORATOR"),
        }
    }

    fn json(&self) -> String {
        format!(
            r#"{{"pull_request_review_id": {}, "path": {}, "line": {}, "original_line": {}, "start_line": {}, "original_start_line": {}, "subject_type": "{}", "created_at": "{at}", "updated_at": "{at}", "html_url": "https://example.test/i/{}", "body": {}, "user": {}, "author_association": {}}}"#,
            self.review_id,
            serde_json::json!(self.path),
            serde_json::json!(self.line),
            serde_json::json!(self.original_line),
            serde_json::json!(self.start_line),
            serde_json::json!(self.original_start_line),
            self.subject,
            self.body.len(),
            serde_json::json!(self.body),
            serde_json::json!(self.login.map(|l| serde_json::json!({ "login": l }))),
            serde_json::json!(self.assoc),
            at = self.at
        )
    }
}

fn inline_list(items: &[Inline]) -> String {
    list(&items.iter().map(Inline::json).collect::<Vec<_>>())
}

fn review_fixture(comments: &[String], reviews: &[String], inline: &[Inline]) -> Fixture {
    let fx = Fixture::new(SINCE, &[list(comments)]);
    fx.reviews(&[list(reviews)]);
    fx.review_comments(&[inline_list(inline)]);
    fx
}

fn review_stdout(comments: &[String], reviews: &[String], inline: &[Inline]) -> String {
    let fx = review_fixture(comments, reviews, inline);
    let out = fx.run();
    assert!(
        out.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

fn owner_comment(at: &str, body: &str) -> String {
    comment(at, "OWNER", Some("me"), body)
}

#[test]
fn a_changes_requested_review_requests_changes_and_hands_over_its_inline_comments() {
    let out = review_stdout(
        &[],
        &[rev("CHANGES_REQUESTED", FRESH, "")],
        &[
            Inline::at_line(11, "src/a.rs", 5, "fix the first"),
            Inline::at_line(11, "src/b.rs", 9, "fix the second"),
        ],
    );
    assert!(out.starts_with("REQUEST_CHANGES\n\n"), "{out}");
    for s in [
        format!("### rev (COLLABORATOR), {FRESH}, review CHANGES_REQUESTED").as_str(),
        "#### src/a.rs:5",
        "https://example.test/i/13",
        "fix the first",
        "#### src/b.rs:9",
        "fix the second",
    ] {
        assert!(out.contains(s), "{s} missing:\n{out}");
    }
}

#[test]
fn an_approved_review_approves() {
    let out = review_stdout(
        &[],
        &[review("11", "APPROVED", Some(FRESH), "OWNER", "me", "")],
        &[],
    );
    assert!(out.starts_with("APPROVE\n\n"), "{out}");
}

#[test]
fn a_markerless_comment_review_gives_no_verdict_and_renders_nothing() {
    let fx = review_fixture(
        &[],
        &[rev("COMMENTED", FRESH, "looks fine")],
        &[Inline::at_line(11, "a.rs", 1, "hm")],
    );
    let out = fx.run();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let calls = fx.calls();
    assert_eq!(calls.matches("pulls/7/reviews").count(), 1, "{calls}");
    assert!(!calls.contains("pulls/7/comments"), "{calls}");
}

#[test]
fn a_commented_review_votes_through_a_marker_line_in_its_body() {
    for (body, want) in [
        ("note\n/request-changes", "REQUEST_CHANGES"),
        ("note\n/approve", "APPROVE"),
    ] {
        let out = review_stdout(&[], &[rev("COMMENTED", FRESH, body)], &[]);
        assert_eq!(first_line(&out), want, "{body}");
    }
}

/// The review from PR #229: a typo'd marker plus 7 inline comments.
#[test]
fn the_229_review_with_a_typo_marker_is_not_a_verdict() {
    let body =
        "/request-change\n\ngo through the comments in this review and implement requested changes";
    let mut inline: Vec<Inline> = Vec::new();
    for (i, l) in [23, 33, 40, 35, 71, 73].into_iter().enumerate() {
        inline.push(Inline {
            line: None,
            original_line: Some(l),
            at: STALE,
            ..Inline::at_line(11, "src/x.rs", i as u64, "old")
        });
    }
    inline.push(Inline {
        at: STALE,
        ..Inline::at_line(11, "src/y.rs", 31, "current")
    });
    let out = review_stdout(
        &[],
        &[review("11", "COMMENTED", Some(FRESH), "OWNER", "me", body)],
        &inline,
    );
    assert_eq!(out, "");
}

#[test]
fn a_review_older_than_the_head_commit_never_votes() {
    for (state, body) in [
        ("APPROVED", ""),
        ("CHANGES_REQUESTED", ""),
        ("COMMENTED", "/approve"),
    ] {
        let out = review_stdout(&[], &[rev(state, STALE, body)], &[]);
        assert_eq!(out, "", "{state}");
    }
}

#[test]
fn a_review_outside_the_author_fence_never_votes() {
    for (assoc, login) in [("CONTRIBUTOR", "me"), ("NONE", "me"), ("OWNER", "ci[bot]")] {
        let out = review_stdout(
            &[],
            &[review("11", "APPROVED", Some(FRESH), assoc, login, "")],
            &[],
        );
        assert_eq!(out, "", "{assoc} {login}");
    }
}

#[test]
fn pending_and_dismissed_reviews_never_vote() {
    let pending = review("11", "PENDING", None, "OWNER", "me", "/approve");
    assert_eq!(review_stdout(&[], &[pending], &[]), "");
    let dismissed = review("11", "DISMISSED", Some(FRESH), "OWNER", "me", "/approve");
    assert_eq!(review_stdout(&[], &[dismissed], &[]), "");
}

#[test]
fn an_approved_review_with_a_request_changes_line_requests_changes() {
    let out = review_stdout(&[], &[rev("APPROVED", FRESH, "/request-changes")], &[]);
    assert_eq!(first_line(&out), "REQUEST_CHANGES");
}

#[test]
fn the_newest_item_across_comments_and_reviews_decides() {
    let cases = [
        (
            owner_comment(LATER, "/approve"),
            "CHANGES_REQUESTED",
            FRESH,
            "APPROVE",
        ),
        (
            owner_comment(FRESH, "/request-changes"),
            "APPROVED",
            LATER,
            "APPROVE",
        ),
        (
            owner_comment(FRESH, "/approve"),
            "CHANGES_REQUESTED",
            LATER,
            "REQUEST_CHANGES",
        ),
        (
            owner_comment(FRESH, "/approve"),
            "CHANGES_REQUESTED",
            FRESH,
            "REQUEST_CHANGES",
        ),
        (
            owner_comment(FRESH, "/request-changes"),
            "APPROVED",
            FRESH,
            "REQUEST_CHANGES",
        ),
    ];
    for (c, state, at, want) in cases {
        let out = review_stdout(std::slice::from_ref(&c), &[rev(state, at, "")], &[]);
        assert_eq!(first_line(&out), want, "{c} vs {state} at {at}");
    }
}

/// A comment created at `created` and edited at `updated`.
fn edited_comment(created: &str, updated: &str, body: &str) -> String {
    comment(created, "OWNER", Some("me"), body).replace(
        &format!(r#""updated_at": "{created}""#),
        &format!(r#""updated_at": "{updated}""#),
    )
}

#[test]
fn a_comment_votes_at_the_time_it_was_last_edited() {
    // Created before the review, edited after it: the edit is the newest vote.
    let c = edited_comment(STALE, LATER, "/approve");
    let out = review_stdout(&[c], &[rev("CHANGES_REQUESTED", FRESH, "")], &[]);
    assert_eq!(first_line(&out), "APPROVE");
    // Two comments: the older one, edited last, wins.
    let a = edited_comment(FRESH, LATER, "/approve");
    let b = owner_comment("2026-01-03T12:00:00Z", "/request-changes");
    let out = review_stdout(&[a, b], &[], &[]);
    assert_eq!(first_line(&out), "APPROVE");
}

#[test]
fn inline_positions_cover_ranges_single_lines_and_bare_paths() {
    let outdated_range = Inline {
        line: None,
        original_line: Some(23),
        original_start_line: Some(20),
        ..Inline::at_line(11, "p.rs", 0, "A")
    };
    let same = Inline {
        start_line: Some(5),
        ..Inline::at_line(11, "q.rs", 5, "B")
    };
    let bare = Inline {
        line: None,
        original_line: None,
        ..Inline::at_line(11, "r.rs", 0, "C")
    };
    let out = review_stdout(
        &[],
        &[rev("CHANGES_REQUESTED", FRESH, "")],
        &[outdated_range, same, bare],
    );
    for s in [
        "#### p.rs:20-23 (outdated)\n",
        "#### q.rs:5\n",
        "#### r.rs\n",
    ] {
        assert!(out.contains(s), "{s} missing:\n{out}");
    }
}

#[test]
fn reviews_submitted_together_are_ordered_by_id() {
    let a = review("30", "COMMENTED", Some(FRESH), "OWNER", "me", "THIRTY");
    let b = review("4", "CHANGES_REQUESTED", Some(FRESH), "OWNER", "me", "FOUR");
    let out = review_stdout(&[], &[a, b], &[]);
    assert!(
        out.find("FOUR").unwrap() < out.find("THIRTY").unwrap(),
        "{out}"
    );
}

#[test]
fn a_later_top_level_marker_after_a_typo_review_hands_over_both() {
    let out = review_stdout(
        &[owner_comment(LATER, "/request-changes")],
        &[review(
            "11",
            "COMMENTED",
            Some(FRESH),
            "OWNER",
            "me",
            "/request-change\n\ngo through the comments",
        )],
        &[
            Inline::at_line(11, "a.rs", 1, "INLINE-ONE"),
            Inline::at_line(11, "a.rs", 2, "INLINE-TWO"),
            Inline::at_line(11, "a.rs", 3, "INLINE-THREE"),
        ],
    );
    assert!(out.starts_with("REQUEST_CHANGES\n\n"), "{out}");
    let c = out.find("### me (OWNER), ").unwrap();
    let r = out.find("review COMMENTED").unwrap();
    assert!(c < r, "comments come before reviews:\n{out}");
    for s in ["INLINE-ONE", "INLINE-TWO", "INLINE-THREE"] {
        assert!(out[r..].contains(s), "{s} missing:\n{out}");
    }
}

#[test]
fn inline_comments_render_their_position_and_follow_their_review() {
    let outdated = Inline {
        line: None,
        original_line: Some(23),
        ..Inline::at_line(11, "p.rs", 0, "OUTDATED")
    };
    let file = Inline {
        subject: "file",
        line: None,
        original_line: None,
        ..Inline::at_line(11, "f.rs", 0, "FILEWIDE")
    };
    let range = Inline {
        start_line: Some(10),
        ..Inline::at_line(11, "r.rs", 12, "RANGE")
    };
    let stale = Inline {
        at: STALE,
        ..Inline::at_line(11, "s.rs", 4, "STALE-BUT-KEPT")
    };
    // A fresh inline comment of a review that does not qualify.
    let other = review("12", "COMMENTED", Some(STALE), "OWNER", "old", "earlier");
    let foreign = Inline::at_line(12, "z.rs", 1, "NOT-INCLUDED");
    let out = review_stdout(
        &[],
        &[rev("CHANGES_REQUESTED", FRESH, ""), other],
        &[outdated, file, range, stale, foreign],
    );
    for s in [
        "#### p.rs:23 (outdated)\n",
        "#### f.rs (file)\n",
        "#### r.rs:10-12\n",
        "STALE-BUT-KEPT",
    ] {
        assert!(out.contains(s), "{s} missing:\n{out}");
    }
    assert!(!out.contains("NOT-INCLUDED"), "{out}");
    assert!(!out.contains("review COMMENTED"), "{out}");
}

#[test]
fn reviews_are_rendered_oldest_first() {
    let a = review("21", "COMMENTED", Some(LATER), "OWNER", "later", "second");
    let b = review(
        "22",
        "CHANGES_REQUESTED",
        Some(FRESH),
        "OWNER",
        "early",
        "first",
    );
    let out = review_stdout(&[], &[a, b], &[]);
    assert!(
        out.find("first").unwrap() < out.find("second").unwrap(),
        "{out}"
    );
}

#[test]
fn a_gh_failure_in_any_review_call_is_an_error_with_empty_stdout() {
    for what in ["reviews-1", "reviews-2", "reviews-3", "review-comments-1"] {
        let fx = review_fixture(
            &[owner_comment(FRESH, "/request-changes")],
            &[rev("COMMENTED", FRESH, "")],
            &[Inline::at_line(11, "a.rs", 1, "x")],
        );
        fx.fail(what);
        let out = fx.run();
        assert!(!out.status.success(), "{what} must fail the script");
        assert!(out.stdout.is_empty(), "{what}: no partial verdict");
        assert!(!out.stderr.is_empty(), "{what}: stderr must say why");
    }
}

#[test]
fn a_non_numeric_review_id_fails_closed() {
    let fx = review_fixture(
        &[owner_comment(FRESH, "/request-changes")],
        &[review("abc", "COMMENTED", Some(FRESH), "OWNER", "me", "")],
        &[],
    );
    let out = fx.run();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}

#[test]
fn a_merged_pr_never_reads_reviews() {
    let fx = review_fixture(&[], &[rev("CHANGES_REQUESTED", FRESH, "")], &[]);
    fx.pr_state("MERGED");
    let out = fx.run();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("MERGED\n")
    );
    let calls = fx.calls();
    for s in ["pulls/7/reviews", "pulls/7/comments", "issues/7/comments"] {
        assert!(!calls.contains(s), "{s} was read: {calls}");
    }
}

#[test]
fn an_oversized_review_rendering_is_truncated_at_a_whole_item_header() {
    let big = "x".repeat(60_000);
    let reviews: Vec<String> = (0..12)
        .map(|i| {
            review(
                &format!("{}", 100 + i),
                "COMMENTED",
                Some(FRESH),
                "OWNER",
                "me",
                &big,
            )
        })
        .collect();
    let inline: Vec<Inline> = (0..12)
        .map(|i| Inline::at_line(100 + i, "a.rs", 1, "inline body"))
        .collect();
    let cs = vec![owner_comment(LATER, "/request-changes")];
    let out = review_stdout(&cs, &reviews, &inline);
    assert!(out.starts_with("REQUEST_CHANGES\n\n"));
    assert!(out.len() < 600_000, "{}", out.len());
    assert!(out.contains("[truncated"));
    assert!(out.contains("pulls/7/reviews") && out.contains("pulls/7/comments"));
    let (_, after) = out.split_once("]\n\n").expect("no truncation note");
    let head = first_line(after);
    assert!(head.starts_with("### me (OWNER), "), "{head}");
    assert!(head.contains(", review COMMENTED"), "{head}");
}

#[test]
fn the_script_agrees_with_the_marker_table_for_review_bodies() {
    for case in marker_cases() {
        let out = review_stdout(&[], &[rev("COMMENTED", FRESH, &case.body)], &[]);
        assert_eq!(
            first_line(&out),
            case.github,
            "case '{}': body {:?}",
            case.name,
            case.body
        );
    }
}

/// A review dismissed between the id list and its header call prints nothing:
/// it is skipped with its inline comments, and that is not an error.
#[test]
fn a_review_that_stops_qualifying_mid_run_is_skipped_with_its_inline_comments() {
    let fx = review_fixture(
        &[owner_comment(FRESH, "/request-changes")],
        &[rev("COMMENTED", FRESH, "")],
        &[Inline::at_line(11, "a.rs", 1, "GONE-INLINE")],
    );
    fs::write(fx.dir.join("empty-fail-reviews-3"), "").unwrap();
    let out = fx.run();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("REQUEST_CHANGES\n\n"), "{stdout}");
    assert!(!stdout.contains("review COMMENTED"), "{stdout}");
    assert!(!stdout.contains("GONE-INLINE"), "{stdout}");
    assert!(!fx.calls().contains("pulls/7/comments"));
}

fn inline_by(
    id: u64,
    path: &'static str,
    line: u64,
    body: &'static str,
    login: Option<&'static str>,
    assoc: Option<&'static str>,
) -> Inline<'static> {
    Inline {
        login,
        assoc,
        ..Inline::at_line(id, path, line, body)
    }
}

#[test]
fn an_owners_request_changes_review_hands_over_every_inline_comment() {
    let out = review_stdout(
        &[],
        &[review(
            "11",
            "COMMENTED",
            Some(FRESH),
            "OWNER",
            "me",
            "/request-changes",
        )],
        &[
            inline_by(11, "src/a.rs", 5, "OWNER-ONE", Some("me"), Some("OWNER")),
            inline_by(11, "src/b.rs", 9, "OWNER-TWO", Some("me"), Some("OWNER")),
        ],
    );
    assert_eq!(first_line(&out), "REQUEST_CHANGES");
    assert!(out.starts_with("REQUEST_CHANGES\n\n"), "{out}");
    let header = out
        .find(&format!("### me (OWNER), {FRESH}, review COMMENTED"))
        .unwrap_or_else(|| panic!("{out}"));
    for s in [
        "#### src/a.rs:5",
        "OWNER-ONE",
        "#### src/b.rs:9",
        "OWNER-TWO",
    ] {
        assert!(out.find(s).unwrap_or_else(|| panic!("{s}: {out}")) > header);
    }
}

#[test]
fn inline_comments_from_outsiders_are_dropped() {
    for assoc in ["CONTRIBUTOR", "NONE", "FIRST_TIMER"] {
        let body: &'static str = Box::leak(format!("OUTSIDER-{assoc}").into_boxed_str());
        let out = review_stdout(
            &[],
            &[rev("CHANGES_REQUESTED", FRESH, "")],
            &[
                Inline::at_line(11, "src/ok.rs", 3, "QUALIFYING"),
                inline_by(11, "src/out.rs", 4, body, Some("outsider"), Some(assoc)),
            ],
        );
        assert!(out.contains("QUALIFYING"), "{out}");
        assert!(out.contains("#### src/ok.rs:3"), "{out}");
        assert!(!out.contains(body), "{assoc}: {out}");
    }
}

#[test]
fn inline_comments_from_bots_are_dropped_whatever_their_association() {
    for assoc in ["OWNER", "MEMBER", "COLLABORATOR"] {
        let body: &'static str = Box::leak(format!("BOT-{assoc}").into_boxed_str());
        let out = review_stdout(
            &[],
            &[rev("CHANGES_REQUESTED", FRESH, "")],
            &[
                Inline::at_line(11, "src/ok.rs", 3, "QUALIFYING"),
                inline_by(11, "src/bot.rs", 4, body, Some("ci[bot]"), Some(assoc)),
            ],
        );
        assert!(out.contains("QUALIFYING"), "{out}");
        assert!(!out.contains(body), "{assoc}: {out}");
    }
}

#[test]
fn inline_comments_with_null_or_missing_association_are_dropped_but_a_null_user_is_not() {
    let missing = {
        let mut v: serde_json::Value =
            serde_json::from_str(&Inline::at_line(11, "src/m.rs", 2, "MISSING-ASSOC").json())
                .unwrap();
        v.as_object_mut().unwrap().remove("author_association");
        v.to_string()
    };
    let fx = Fixture::new(SINCE, &[list(&[])]);
    fx.reviews(&[list(&[rev("CHANGES_REQUESTED", FRESH, "")])]);
    fx.review_comments(&[list(&[
        inline_by(11, "src/n.rs", 1, "NULL-ASSOC", Some("rev"), None).json(),
        missing,
        inline_by(11, "src/u.rs", 7, "NULL-USER", None, Some("COLLABORATOR")).json(),
    ])]);
    let out = fx.run();
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(!out.contains("NULL-ASSOC"), "{out}");
    assert!(!out.contains("MISSING-ASSOC"), "{out}");
    assert!(out.contains("NULL-USER"), "{out}");
    assert!(out.contains("#### src/u.rs:7"), "{out}");
}

#[test]
fn dropping_inline_comments_never_changes_the_verdict() {
    let bad = || {
        vec![
            inline_by(11, "a.rs", 1, "X1", Some("o"), Some("CONTRIBUTOR")),
            inline_by(11, "b.rs", 2, "X2", Some("ci[bot]"), Some("OWNER")),
            inline_by(11, "c.rs", 3, "X3", Some("rev"), None),
        ]
    };
    for (state, body, want) in [
        ("CHANGES_REQUESTED", "", "REQUEST_CHANGES"),
        ("APPROVED", "", "APPROVE"),
        ("COMMENTED", "/request-changes", "REQUEST_CHANGES"),
    ] {
        let with = review_stdout(&[], &[rev(state, FRESH, body)], &bad());
        let without = review_stdout(&[], &[rev(state, FRESH, body)], &[]);
        assert_eq!(first_line(&with), want, "{state}: {with}");
        assert_eq!(first_line(&without), want, "{state}: {without}");
        for x in ["X1", "X2", "X3"] {
            assert!(!with.contains(x), "{with}");
        }
    }
    let out = review_stdout(&[], &[rev("COMMENTED", FRESH, "")], &bad());
    assert!(out.is_empty(), "{out}");
}

#[test]
fn a_vote_on_review_page_two_counts_and_its_inline_comments_are_handed_over() {
    let fx = Fixture::new(SINCE, &[list(&[])]);
    fx.reviews(&[
        list(&[rev("COMMENTED", FRESH, "just a note")]),
        list(&[review(
            "12",
            "CHANGES_REQUESTED",
            Some(FRESH),
            "OWNER",
            "me",
            "PAGE-TWO-REVIEW",
        )]),
    ]);
    fx.review_comments(&[inline_list(&[
        inline_by(12, "p.rs", 1, "P2-ONE", Some("me"), Some("OWNER")),
        inline_by(12, "q.rs", 2, "P2-TWO", Some("me"), Some("OWNER")),
    ])]);
    let out = fx.run();
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert_eq!(first_line(&out), "REQUEST_CHANGES", "{out}");
    for s in ["PAGE-TWO-REVIEW", "P2-ONE", "P2-TWO"] {
        assert!(out.contains(s), "{s}: {out}");
    }
}

#[test]
fn inline_comments_split_across_pages_render_in_page_order_after_their_review() {
    let fx = Fixture::new(SINCE, &[list(&[])]);
    fx.reviews(&[list(&[rev("CHANGES_REQUESTED", FRESH, "")])]);
    fx.review_comments(&[
        inline_list(&[Inline::at_line(11, "a.rs", 1, "INLINE-P1")]),
        inline_list(&[Inline::at_line(11, "b.rs", 2, "INLINE-P2")]),
    ]);
    let out = fx.run();
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    let h = out.find("### rev (COLLABORATOR)").expect(&out);
    let p1 = out.find("INLINE-P1").expect(&out);
    let p2 = out.find("INLINE-P2").expect(&out);
    assert!(h < p1 && p1 < p2, "{out}");
}

#[test]
fn the_newest_vote_wins_across_review_pages() {
    for (first, second, want) in [
        ("APPROVED", "CHANGES_REQUESTED", "REQUEST_CHANGES"),
        ("CHANGES_REQUESTED", "APPROVED", "APPROVE"),
    ] {
        let fx = Fixture::new(SINCE, &[list(&[])]);
        fx.reviews(&[
            list(&[rev(first, FRESH, "")]),
            list(&[review("12", second, Some(LATER), "COLLABORATOR", "rev", "")]),
        ]);
        fx.review_comments(&[inline_list(&[])]);
        let out = fx.run();
        assert!(out.status.success());
        let out = String::from_utf8(out.stdout).unwrap();
        assert_eq!(first_line(&out), want, "{first}/{second}: {out}");
    }
}

#[test]
fn a_gh_failure_on_page_two_of_reviews_or_inline_comments_is_an_error_with_empty_stdout() {
    for what in ["reviews-page-2.json", "review-comments-page-2.json"] {
        let fx = Fixture::new(SINCE, &[list(&[owner_comment(FRESH, "/request-changes")])]);
        fx.reviews(&[
            list(&[rev("COMMENTED", FRESH, "/request-changes")]),
            list(&[review("12", "COMMENTED", Some(FRESH), "OWNER", "me", "")]),
        ]);
        fx.review_comments(&[
            inline_list(&[Inline::at_line(11, "a.rs", 1, "P1")]),
            inline_list(&[Inline::at_line(11, "b.rs", 2, "P2")]),
        ]);
        fx.fail(what);
        let out = fx.run();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{what} must fail the script");
        assert!(out.stdout.is_empty(), "{what}: no partial output");
        assert!(err.contains("page 2 failed"), "{what}: {err}");
        assert!(err.contains("choco await-review:"), "{what}: {err}");
    }
}
