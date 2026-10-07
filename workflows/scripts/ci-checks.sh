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
#   RED               a check is FAILURE, ERROR or TIMED_OUT, and the failed
#                     GitHub Actions jobs were already re-run once for this
#                     head or cannot be re-run (see RE-RUN below)
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
# RE-RUN. A red result would send the coder on a paid lap, and a flaky test
# is not the coder's to fix. So for each head, the first time CI is red the
# failed Actions jobs are re-run once (`gh run rerun <run id> --failed`) and
# polling goes on. A failing check is FAILURE, ERROR or TIMED_OUT; it is
# re-runnable if its link is an Actions job URL
# (https://github.com/<owner>/<repo>/actions/runs/<run id>/job/<job id>).
# Applied only when the classification would be RED, in this order:
#   1. A failing check is not re-runnable (external status, third-party app,
#      empty or missing link): RED now. No git directory is needed.
#   2. The record `<git dir>/choco-ci-rerun` is parsable and names this head:
#      every failing link is in it -> PENDING (GitHub has not replaced the
#      failed attempt yet; a re-run job has a new job id, so a new link);
#      any failing link is not in it -> RED (the re-run failed or a new job
#      failed).
#   3. Not re-run yet, but a check is still running (a state outside SUCCESS,
#      SKIPPED, NEUTRAL, FAILURE, ERROR, TIMED_OUT, STARTUP_FAILURE,
#      ACTION_REQUIRED, CANCELLED): PENDING. GitHub refuses to re-run a run
#      that is in progress, and one re-run can then cover every failure.
#   4. Not re-run yet and everything finished: `gh run rerun <id> --failed`
#      once per distinct run id (sort -u order, stopping at the first
#      failure), then the record is written, and PENDING. If a re-run call or
#      the record write fails: RED, with the reason on stderr.
# The record: line 1 is the head sha, then one failing link per line, sorted
# and unique, written whole (temp file, then `mv`) only after every re-run
# call succeeded. An empty record, a line 1 that is empty or has whitespace,
# no link lines or a link that is not an Actions job URL is unparsable: a
# note on stderr, then it counts as no record and rule 4 rewrites it. A
# record for another sha counts as no record. STARTUP_FAILURE and the other
# tokens never win over a red: a red alongside them prints PENDING or RED.
#
# FAIL CLOSED. Any `gh` or `git` failure, an empty head, a count that is not
# a number, or a failed write of the first-seen file: a note on stderr
# (`choco ci-checks: ...`), exit 1 and nothing on stdout. A failure must
# never print a token, and never look like NO_CHECKS or GREEN. The one
# exception is a failed `gh run rerun` or a failed write of the re-run
# record: those end in RED (today's behaviour) with a note on stderr.
set -eu
LC_ALL=C
export LC_ALL

NO_CHECKS_GRACE=180

TMP=$(mktemp -d) || { printf 'choco ci-checks: mktemp failed\n' >&2; exit 1; }
FS_TMP=""
REC_TMP=""
trap 'rm -rf "$TMP"; [ -z "$FS_TMP" ] || rm -f "$FS_TMP"; [ -z "$REC_TMP" ] || rm -f "$REC_TMP"' EXIT

fail() {
    printf 'choco ci-checks: %s\n' "$1" >&2
    exit 1
}

# Prints the token, a blank line and the check lines; then, if given, a blank
# line and a message. Exits 0.
emit() {
    printf '%s\n\n' "$1"
    cat "$TMP/checks"
    if [ -n "$2" ]; then
        printf '\n%s\n' "$2"
    fi
    exit 0
}

ACTIONS_JOB_URL='^https://github[.]com/[^/]+/[^/]+/actions/runs/[0-9]+/job/[0-9]+$'

# Runs only when the classification is RED: decides between RED and the
# one-time re-run of the failed Actions jobs (see RE-RUN above).
rerun_gate() {
    awk '$1 == "FAILURE" || $1 == "ERROR" || $1 == "TIMED_OUT" { print $2 }' \
        "$TMP/checks.raw" > "$TMP/fail.unsorted" || fail "awk failed"
    sort -u "$TMP/fail.unsorted" > "$TMP/fail.links" || fail "sort failed"
    # Rule 1: a failing check that cannot be re-run is red now.
    awk -v pat="$ACTIONS_JOB_URL" '$1 !~ pat { n++ } END { print n + 0 }' \
        "$TMP/fail.links" > "$TMP/nonactions" || fail "awk failed"
    IFS= read -r NON_ACTIONS < "$TMP/nonactions" || fail "awk printed nothing"
    [ "$NON_ACTIONS" -eq 0 ] || emit RED ""

    # Rule 2: the record for this head.
    GIT_DIR_ABS=$(git rev-parse --absolute-git-dir) || fail "git rev-parse --absolute-git-dir failed"
    [ -n "$GIT_DIR_ABS" ] || fail "no git directory found"
    RECORD="$GIT_DIR_ABS/choco-ci-rerun"
    if [ -e "$RECORD" ]; then
        REC_SHA=""
        IFS= read -r REC_SHA < "$RECORD" || true
        awk -v pat="$ACTIONS_JOB_URL" '
            NR == 1 { if ($0 == "" || $0 ~ /[ \t]/) bad = 1; next }
            { n++; if ($0 !~ pat) bad = 1 }
            END { print ((NR == 0 || n == 0 || bad) ? "BAD" : "OK") }' \
            "$RECORD" > "$TMP/recstate" || fail "awk failed"
        IFS= read -r REC_STATE < "$TMP/recstate" || fail "awk printed nothing"
        if [ "$REC_STATE" != OK ]; then
            printf 'choco ci-checks: ignoring unparsable %s; treating this head as not re-run yet\n' "$RECORD" >&2
        elif [ "$REC_SHA" = "$HEAD_SHA" ]; then
            awk 'NR == FNR { if (FNR > 1) r[$0] = 1; next } !($0 in r) { m++ } END { print m + 0 }' \
                "$RECORD" "$TMP/fail.links" > "$TMP/unrecorded" || fail "awk failed"
            IFS= read -r UNRECORDED < "$TMP/unrecorded" || fail "awk printed nothing"
            if [ "$UNRECORDED" -eq 0 ]; then
                emit PENDING "Waiting for the re-run of the failed jobs for $HEAD_SHA."
            fi
            emit RED ""
        fi
    fi

    # Rule 3: wait for the other checks, so one re-run covers every failure.
    awk '$1 != "SUCCESS" && $1 != "SKIPPED" && $1 != "NEUTRAL" && $1 != "FAILURE" &&
         $1 != "ERROR" && $1 != "TIMED_OUT" && $1 != "STARTUP_FAILURE" &&
         $1 != "ACTION_REQUIRED" && $1 != "CANCELLED" { n++ } END { print n + 0 }' \
        "$TMP/checks.raw" > "$TMP/running" || fail "awk failed"
    IFS= read -r RUNNING < "$TMP/running" || fail "awk printed nothing"
    if [ "$RUNNING" -gt 0 ]; then
        emit PENDING "A check failed; waiting for the other checks to finish before re-running the failed jobs."
    fi

    # Rule 4: re-run the failed jobs of each run once, then record it.
    sed -n 's|^.*/actions/runs/\([0-9][0-9]*\)/job/.*$|\1|p' "$TMP/fail.links" > "$TMP/runs.unsorted" ||
        fail "sed failed"
    sort -u "$TMP/runs.unsorted" > "$TMP/runs" || fail "sort failed"
    RUN_IDS=""
    for RUN_ID in $(cat "$TMP/runs"); do
        RC=0
        gh run rerun "$RUN_ID" --failed < /dev/null > /dev/null || RC=$?
        if [ "$RC" -ne 0 ]; then
            printf 'choco ci-checks: re-running the failed jobs of run %s failed (exit %s); reporting RED\n' "$RUN_ID" "$RC" >&2
            emit RED ""
        fi
        RUN_IDS="$RUN_IDS $RUN_ID"
    done
    WHY=""
    write_record() {
        REC_TMP=$(mktemp "$GIT_DIR_ABS/choco-ci-rerun.XXXXXX") || {
            WHY="cannot create a temp file in $GIT_DIR_ABS"
            return 1
        }
        { printf '%s\n' "$HEAD_SHA"; cat "$TMP/fail.links"; } > "$REC_TMP" || {
            WHY="cannot write $REC_TMP"
            return 1
        }
        mv -f "$REC_TMP" "$RECORD" || {
            WHY="cannot move $REC_TMP to $RECORD"
            return 1
        }
        REC_TMP=""
    }
    if ! write_record; then
        printf 'choco ci-checks: re-ran the failed jobs but could not record it in %s: %s; reporting RED\n' "$RECORD" "$WHY" >&2
        emit RED ""
    fi
    emit PENDING "Re-ran the failed jobs of run${RUN_IDS} for $HEAD_SHA."
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
    gh pr checks "$PR_NUMBER" --json name,state,link \
        -q '.[] | "\(.state) \((.link // "") | if . == "" then "-" else gsub(" "; "%20") end) \(.name)"' \
        > "$TMP/checks.raw" ||
        fail "gh pr checks failed"
    [ -s "$TMP/checks.raw" ] || fail "gh pr checks printed nothing although $COUNT checks exist"
    # Lines are `<STATE> <link or -> <name>`; the shown lines drop the link.
    sed 's/^\([^ ]*\) [^ ]* /\1 /' "$TMP/checks.raw" > "$TMP/checks.unsorted" || fail "sed failed"
    sort "$TMP/checks.unsorted" > "$TMP/checks" || fail "sort failed"
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
        }' "$TMP/checks.raw" > "$TMP/token" || fail "awk failed"
    IFS= read -r TOKEN < "$TMP/token" || fail "no classification produced"
    case "$TOKEN" in
    RED | STARTUP_FAILURE | ACTION_REQUIRED | CANCELLED | GREEN | PENDING) ;;
    *) fail "unexpected classification '$TOKEN'" ;;
    esac
    if [ "$TOKEN" = RED ]; then
        rerun_gate
    fi
    emit "$TOKEN" ""
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
