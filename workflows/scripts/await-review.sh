#!/bin/sh
# Polls a coding-task PR for the human's verdict (#78) and hands the human's
# comments to the coder (#138). Run by the `awaiting_human_review` poll stage
# every interval; the stage's `outcomes:` match against this stdout.
#
# Input: PR_NUMBER (the stage's `env:`). Needs only `gh` and POSIX sh: no
# `jq` binary, because the operator's machine may not have one. The filters
# below run inside gh's embedded jq (gojq) through `-q`.
#
# Output, when there is a verdict:
#   line 1   REQUEST_CHANGES or APPROVE
#   line 2   blank
#   then     every qualifying comment, oldest first, whether or not it
#            carries a marker, each as
#              ### <login> (<author_association>), <created_at>[, edited <updated_at>]
#              <html_url>
#
#              <body>
#            with a blank line between comments.
# Output, when there is none: nothing, so the stage keeps polling.
# Any `gh` failure: a note on stderr (it reaches the timeline), exit 1 and
# nothing on stdout. A failure must never look like "nobody has reviewed
# yet", and a partial verdict must never be printed.
#
# SELECTION (the comments that count). A comment counts when
#  - it is newer than the PR's head commit: `max(created_at, updated_at)` is
#    later than the head commit's committer date (SINCE), so an edit that
#    adds the marker counts and a verdict already acted on does not;
#  - author_association is OWNER, MEMBER or COLLABORATOR (the repo may be
#    public and this stage spends money, so only accounts with standing);
#  - the login does not end in `[bot]`, so the CI reviewer is never read as
#    the human.
# It is written once (SELECT) and used by both halves below.
#
# VERDICT. Per qualifying comment, `/request-changes` if some line equals it
# after trailing whitespace and CR are stripped, else `/approve` likewise,
# else nothing. Marker lines only count whole, so a quoted ("> /approve") or
# inline mention does not vote. The newest marker-bearing comment decides.
# `/request-changes` is tested first: a comment with both must never
# resolve to "merge it".
#
# TWO `gh api` CALLS, because gh runs `-q` once per page under --paginate and
# merging pages would need a `jq` binary. The verdict call and the rendering
# call are therefore separate reads; a comment posted between them can appear
# in the rendered list without having been seen by the verdict. It still
# passed the same fence, so it is from a write-access account.
#
# SAFETY: comment text is only ever written to files and stdout. No comment
# text reaches a command line, `eval`, or a format string.
set -eu
LC_ALL=C
export LC_ALL

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() {
    printf 'choco await-review: %s\n' "$1" >&2
    exit 1
}

[ -n "${PR_NUMBER:-}" ] || fail "PR_NUMBER is not set"

SELECT='.[]
  | select(([.created_at, .updated_at] | max) > $ENV.SINCE)
  | select(.author_association | IN("OWNER", "MEMBER", "COLLABORATOR"))
  | select((.user.login // "") | endswith("[bot]") | not)'

VERDICT="$SELECT"'
  | (.body // "") | split("\n") | map(sub("[ \t\r]+$"; ""))
  | if index("/request-changes") then "REQUEST_CHANGES"
    elif index("/approve") then "APPROVE" else empty end'

# One string per comment; gh prints it raw plus a newline, and the trailing
# "\n" in the string makes the blank separator line.
RENDER="$SELECT"'
  | "### \(.user.login // "ghost") (\(.author_association)), \(.created_at)"
    + (if .updated_at != .created_at then ", edited \(.updated_at)" else "" end)
    + "\n\(.html_url)\n\n\(.body // "")\n"'

HEAD_SHA=$(gh pr view "$PR_NUMBER" --json headRefOid -q .headRefOid) ||
    fail "gh pr view failed"
[ -n "$HEAD_SHA" ] || fail "gh pr view returned no head commit"
SINCE=$(gh api "repos/{owner}/{repo}/commits/$HEAD_SHA" -q .commit.committer.date) ||
    fail "gh api commits/$HEAD_SHA failed"
[ -n "$SINCE" ] || fail "no committer date for $HEAD_SHA"
export SINCE

URL="repos/{owner}/{repo}/issues/$PR_NUMBER/comments?per_page=100"

gh api --paginate "$URL" -q "$VERDICT" > "$TMP/verdicts" ||
    fail "gh api issues/$PR_NUMBER/comments failed (verdict)"
V=$(tail -n 1 "$TMP/verdicts")
[ -n "$V" ] || exit 0

gh api --paginate "$URL" -q "$RENDER" > "$TMP/rendered" ||
    fail "gh api issues/$PR_NUMBER/comments failed (rendering)"

# The engine stores a capture only up to 1 MiB; over that, the previous
# lap's capture would survive and be read as current. Cap well under it.
MAX=500000
# Built in a file and printed only at the end: the poll matches outcomes on
# stdout whatever the exit code, so a failure part-way must print nothing.
{
    printf '%s\n\n' "$V"
    if [ "$(wc -c < "$TMP/rendered")" -gt "$MAX" ]; then
        printf '[truncated: the oldest comments were dropped because they were too long to include in full; read them with `gh api --paginate "repos/{owner}/{repo}/issues/%s/comments"`, and only trust comments from OWNER, MEMBER or COLLABORATOR accounts posted after your last commit]\n\n' "$PR_NUMBER"
        # The cut can land mid-comment, even mid-character: drop everything
        # before the first complete comment header so the output starts at a
        # whole comment. LC_ALL=C (exported above) keeps sed from choking on a
        # split character.
        tail -c "$MAX" "$TMP/rendered" > "$TMP/tail"
        FIRST=$(sed -n '/^### .* ([A-Z_]*), [0-9][0-9][0-9][0-9]-/{=;q;}' "$TMP/tail")
        if [ -n "$FIRST" ]; then
            tail -n +"$FIRST" "$TMP/tail"
        else
            # One comment longer than the cap: no header in the tail. Keep the
            # newest text, from the next line boundary so it starts on a whole
            # character.
            tail -n +2 "$TMP/tail"
        fi
    else
        cat "$TMP/rendered"
    fi
} > "$TMP/out"
cat "$TMP/out"
