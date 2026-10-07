#!/bin/sh
# Decides what a coding-task PR's CI checks mean. Run by the `checks_polling`
# poll stage every interval; the stage's `outcomes:` match against this stdout.
#
# Input: PR_NUMBER (the stage's `env:`). Needs only `gh` and `git` plus POSIX
# sh: no `jq` binary, because the operator's machine may not have one. The
# filters below run inside gh's embedded jq through `-q`.
#
# Output: a token on line 1, a blank line, then one line per check,
# `<STATE> <name>`, sorted, so the timeline shows which check is in which
# state. The workflow's patterns anchor to the token.
#   RED               a check is FAILURE, ERROR or TIMED_OUT
#   STARTUP_FAILURE   a check is STARTUP_FAILURE (and none is red)
#   ACTION_REQUIRED   a check is ACTION_REQUIRED (and none is above)
#   CANCELLED         a check is CANCELLED (and none is above)
#   GREEN             every check is SUCCESS, SKIPPED or NEUTRAL
#   PENDING           anything else (PENDING, QUEUED, IN_PROGRESS, WAITING,
#                     REQUESTED, EXPECTED, STALE, or a state not known here):
#                     the stage keeps polling, and an unknown state therefore
#                     fails closed into the stage's timeout, not into GREEN
#   NO_CHECKS         the PR's head has reported no check for NO_CHECKS_GRACE
#                     seconds (see below)
# The first rule that applies wins, in the order above: a real failure is
# something the coder can read and fix, so it beats everything; the next
# three mean CI cannot go green without a person, so waiting for the other
# checks is pointless.
#
# NO CHECKS. One `gh pr view` read gives the head commit and how many checks
# exist, so "no checks" is never detected from `gh pr checks`' wording. With
# none, the time they were first seen missing for this head is kept in
# `<git dir>/choco-ci-first-seen` as `<head sha> <epoch seconds>`, written
# whole (temp file, then `mv`). No file, an unparsable one or another sha
# starts the clock (a new head, as every revising lap pushes, restarts it)
# and prints PENDING. The same sha with at least NO_CHECKS_GRACE seconds
# elapsed prints NO_CHECKS. Checks that register late still win: the count is
# read every attempt, and once it is above 0 NO_CHECKS is never printed.
#
# FAIL CLOSED. Any `gh` or `git` failure, an empty head, a count that is not
# a number, or a failed write of the first-seen file: a note on stderr
# (`choco ci-checks: ...`), exit 1 and nothing on stdout. A failure must
# never print a token, and never look like NO_CHECKS or GREEN.
set -eu
LC_ALL=C
export LC_ALL

NO_CHECKS_GRACE=180

TMP=$(mktemp -d) || { printf 'choco ci-checks: mktemp failed\n' >&2; exit 1; }
FS_TMP=""
trap 'rm -rf "$TMP"; [ -z "$FS_TMP" ] || rm -f "$FS_TMP"' EXIT

fail() {
    printf 'choco ci-checks: %s\n' "$1" >&2
    exit 1
}

[ -n "${PR_NUMBER:-}" ] || fail "PR_NUMBER is not set"

gh pr view "$PR_NUMBER" --json headRefOid,statusCheckRollup \
    -q '.headRefOid, (.statusCheckRollup | length)' > "$TMP/pr" ||
    fail "gh pr view failed"
{
    IFS= read -r HEAD_SHA || true
    IFS= read -r COUNT || true
} < "$TMP/pr"
[ -n "$HEAD_SHA" ] || fail "gh pr view returned no head commit"
case "$COUNT" in
'' | *[!0-9]*) fail "gh pr view returned a check count that is not a number: '$COUNT'" ;;
esac

if [ "$COUNT" -gt 0 ]; then
    gh pr checks "$PR_NUMBER" --json name,state \
        -q '.[] | "\(.state) \(.name)"' > "$TMP/checks.raw" ||
        fail "gh pr checks failed"
    [ -s "$TMP/checks.raw" ] || fail "gh pr checks printed nothing although $COUNT checks exist"
    sort "$TMP/checks.raw" > "$TMP/checks" || fail "sort failed"
    # One awk pass classifies; its exit status is checked, so a tool failure
    # can never be read as "no failing check".
    awk '
        { s[$1] = 1 }
        END {
            if (("FAILURE" in s) || ("ERROR" in s) || ("TIMED_OUT" in s)) t = "RED"
            else if (("STARTUP_FAILURE" in s)) t = "STARTUP_FAILURE"
            else if (("ACTION_REQUIRED" in s)) t = "ACTION_REQUIRED"
            else if (("CANCELLED" in s)) t = "CANCELLED"
            else {
                t = "GREEN"
                for (k in s) if (k != "SUCCESS" && k != "SKIPPED" && k != "NEUTRAL") t = "PENDING"
            }
            print t
        }' "$TMP/checks" > "$TMP/token" || fail "awk failed"
    IFS= read -r TOKEN < "$TMP/token" || fail "no classification produced"
    case "$TOKEN" in
    RED | STARTUP_FAILURE | ACTION_REQUIRED | CANCELLED | GREEN | PENDING) ;;
    *) fail "unexpected classification '$TOKEN'" ;;
    esac
    printf '%s\n\n' "$TOKEN"
    cat "$TMP/checks"
    exit 0
fi

# No check reported for this head yet.
GIT_DIR_ABS=$(git rev-parse --absolute-git-dir) || fail "git rev-parse --absolute-git-dir failed"
[ -n "$GIT_DIR_ABS" ] || fail "no git directory found"
FIRST_SEEN="$GIT_DIR_ABS/choco-ci-first-seen"
NOW=$(date +%s) || fail "date failed"
case "$NOW" in
'' | *[!0-9]*) fail "date returned a time that is not a number: '$NOW'" ;;
esac

start_clock() {
    FS_TMP=$(mktemp "$GIT_DIR_ABS/choco-ci-first-seen.XXXXXX") ||
        fail "cannot create a temp file in $GIT_DIR_ABS"
    printf '%s %s\n' "$HEAD_SHA" "$NOW" > "$FS_TMP" || fail "cannot write $FS_TMP"
    mv -f "$FS_TMP" "$FIRST_SEEN" || fail "cannot move $FS_TMP to $FIRST_SEEN"
    FS_TMP=""
}

pending() {
    printf 'PENDING\n\nNo checks are reported yet for %s.\n' "$HEAD_SHA"
    exit 0
}

if [ ! -e "$FIRST_SEEN" ]; then
    start_clock
    pending
fi
SEEN_SHA=""; SEEN_AT=""; SEEN_EXTRA=""
read -r SEEN_SHA SEEN_AT SEEN_EXTRA < "$FIRST_SEEN" || true
case "$SEEN_AT" in
'' | *[!0-9]*) SEEN_AT="" ;;
esac
if [ -z "$SEEN_SHA" ] || [ -z "$SEEN_AT" ] || [ -n "$SEEN_EXTRA" ]; then
    printf 'choco ci-checks: ignoring unparsable %s; starting the clock again\n' "$FIRST_SEEN" >&2
    start_clock
    pending
fi
if [ "$SEEN_SHA" != "$HEAD_SHA" ]; then
    start_clock
    pending
fi
ELAPSED=$((NOW - SEEN_AT))
if [ "$ELAPSED" -ge "$NO_CHECKS_GRACE" ]; then
    printf 'NO_CHECKS\n\nNo CI checks reported on this PR'"'"'s head %s after %ss.\n' "$HEAD_SHA" "$ELAPSED"
    exit 0
fi
pending
