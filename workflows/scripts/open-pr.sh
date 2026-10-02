#!/bin/sh
# Opens (or refreshes) the pull request for a `coding-task` branch (#101).
#
# SAFETY: no agent- or human-written text may ever be parsed by a shell. The
# task title and the internal review's report arrive as environment
# variables (the stage's `env:` map), and the coder's description arrives as
# a file in git's private directory for this worktree. Every such value is
# expanded only inside double quotes, printed only as a `printf` argument,
# and handed to `gh` as `--title="$T"` / `--body-file` — never through
# `eval`, `echo`, a format string, or a string that gets re-parsed.
#
# stdout carries ONLY the final read-back JSON (the stage is
# `capture: json`). Everything else goes to /dev/null, a file, or stderr.
# Notes are collected and written to stderr just before the read-back, so
# the 2 KB `stderr_tail` on the timeline keeps them.
#
# Inputs: PR_TASK_TITLE, PR_REVIEW_VERDICT, PR_REVIEW_REPORT (stage `env:`);
# CHOCO_TASK_ID, CHOCO_WORKFLOW, CHOCO_ROLE_MODELS (the engine).
set -eu
LC_ALL=C
export LC_ALL

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

BEGIN='<!-- choco:pr-description:begin -->'
END='<!-- choco:pr-description:end -->'
MARK_BEGIN=$BEGIN
MARK_END=$END
export MARK_BEGIN MARK_END

die() {
    printf 'choco open-pr: %s\n' "$1" >&2
    exit 1
}

note() {
    printf 'choco open-pr: note: %s\n' "$1" >> "$TMP/notes"
}

# Deletes every line that matches a marker (equal to it after trailing
# spaces, tabs and CRs are removed), so text from the coder or the reviewer
# can never break the next lap's splice.
strip_markers() {
    awk '
        function trimmed(l) { sub(/[ \t\r]+$/, "", l); return l }
        {
            t = trimmed($0)
            if (t == ENVIRON["MARK_BEGIN"] || t == ENVIRON["MARK_END"]) next
            print
        }'
}

# Reads stdin, writes at most $1 bytes of it, never ending in the middle of
# a UTF-8 character: it drops any trailing continuation bytes (\200-\277)
# and then one lead byte (\300-\367). Dropping one whole trailing character
# is acceptable.
utf8_head() {
    head -c "$1" > "$TMP/head"
    size=$(wc -c < "$TMP/head" | tr -d ' ')
    drop=$(tail -c 4 "$TMP/head" | od -An -v -tu1 | awk '
        { for (i = 1; i <= NF; i++) { n++; b[n] = $i } }
        END {
            d = 0; i = n
            while (i >= 1 && b[i] >= 128 && b[i] <= 191) { d++; i-- }
            if (i >= 1 && b[i] >= 192 && b[i] <= 247) d++
            print d + 0
        }')
    head -c "$((size - drop))" "$TMP/head"
}

# Passes stdin through if it is at most $1 bytes; otherwise outputs the
# first $1 bytes (cut at a character boundary) and a line saying so.
cap() {
    cat > "$TMP/cap-in"
    total=$(wc -c < "$TMP/cap-in" | tr -d ' ')
    if [ "$total" -le "$1" ]; then
        cat "$TMP/cap-in"
        return 0
    fi
    utf8_head "$1" < "$TMP/cap-in" > "$TMP/cap-out"
    kept=$(wc -c < "$TMP/cap-out" | tr -d ' ')
    cat "$TMP/cap-out"
    printf '\n[truncated: %s of %s bytes shown]\n' "$kept" "$total"
}

# Replaces the generated block in $1 (the PR's current body) with $2 (the
# new block), keeping every other line byte for byte. Exits nonzero, with
# no output, unless $1 has exactly one BEGIN line followed by exactly one
# END line. No content travels through `-v`: both files are read by awk.
splice() {
    awk '
        function trimmed(l) { sub(/[ \t\r]+$/, "", l); return l }
        FILENAME == ARGV[1] {
            cur[++n] = $0
            t = trimmed($0)
            if (t == ENVIRON["MARK_BEGIN"]) { nb++; bi = n }
            else if (t == ENVIRON["MARK_END"]) { ne++; ei = n }
            next
        }
        { blk[++m] = $0 }
        END {
            if (nb != 1 || ne != 1 || ei < bi) exit 1
            for (i = 1; i < bi; i++) print cur[i]
            for (i = 1; i <= m; i++) print blk[i]
            for (i = ei + 1; i <= n; i++) print cur[i]
        }' "$1" "$2"
}

git push -u origin HEAD >/dev/null
BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Title: newlines, CRs and tabs become spaces, runs of spaces collapse to
# one, and the ends are trimmed.
T=$(printf '%s' "${PR_TASK_TITLE:-}" | tr '\n\r\t' '   ' | tr -s ' ' | sed 's/^ //; s/ $//')
[ -n "$T" ] || die "the task has no title; refusing to open a PR named after the branch"

ISSUE=$(printf '%s' "$T" | sed -n 's/.*(#\([0-9][0-9]*\))$/\1/p')
if [ -n "$ISSUE" ]; then
    ISSUE_LINE="Closes #$ISSUE"
else
    FIRST=$(printf '%s' "$T" | awk '{ if (match($0, /#[0-9]+/)) print substr($0, RSTART + 1, RLENGTH - 1) }')
    if [ -n "$FIRST" ]; then
        ISSUE_LINE="Refs #$FIRST"
    else
        ISSUE_LINE="No linked issue: the task's title names none."
        note "no issue number in the task title"
    fi
fi

# GitHub caps a title at 256 characters, and a long one is unreadable in a
# list: keep it to 100 bytes, preserving a trailing "(#N)".
TLEN=$(printf '%s' "$T" | wc -c | tr -d ' ')
if [ "$TLEN" -gt 100 ]; then
    SUFFIX=""
    if [ -n "$ISSUE" ]; then
        SUFFIX=" (#$ISSUE)"
    fi
    BASE=${T%"$SUFFIX"}
    SLEN=$(printf '%s' "$SUFFIX" | wc -c | tr -d ' ')
    HEAD=$(printf '%s' "$BASE" | utf8_head "$((100 - 3 - SLEN))")
    ELLIPSIS=$(printf '\342\200\246')
    T="$HEAD$ELLIPSIS$SUFFIX"
fi

# The description lives in git's private directory for this worktree
# (`.git/worktrees/<name>` in a linked worktree, `.git` in the main one), so
# it is never inside the work tree and never committed. Deliberately not
# `rev-parse` with a path-format flag: Apple Git 2.30 doesn't know it and
# prints the flag back as an extra output line while still exiting 0.
DESC="$(cd "$(git rev-parse --git-dir)" && pwd)/choco-pr-description.md"

{
    printf '%s\n' "$BEGIN"
    printf '%s\n' "$ISSUE_LINE"
    printf '\n'
    if [ -f "$DESC" ] && grep -q '[^[:space:]]' "$DESC"; then
        strip_markers < "$DESC" | cap 16384
    else
        printf '%s\n' '> **The coder wrote no description for this PR.** The internal review below and the commits are the record of what changed.'
        note "no PR description at $DESC"
    fi
    printf '\n'
    printf '%s\n' '## Internal review'
    printf '\n'
    if [ -n "${PR_REVIEW_REPORT:-}" ]; then
        printf '%s\n' "Verdict: **${PR_REVIEW_VERDICT:-unknown}**. The reviewer's final report follows; on a re-review its \"Prior findings\" section says how each earlier finding was resolved."
        printf '\n'
        printf '%s\n' '<details>'
        printf '%s\n' '<summary>Internal reviewer'"'"'s report</summary>'
        printf '\n'
        printf '%s\n' "$PR_REVIEW_REPORT" | strip_markers | cap 40960
        printf '\n'
        printf '%s\n' '</details>'
    else
        printf '%s\n' 'No internal review report was captured for this task.'
    fi
    printf '\n'
    printf '%s\n' '---'
    printf 'Opened by choco task `%s` · workflow `%s` · %s\n' "${CHOCO_TASK_ID:-}" "${CHOCO_WORKFLOW:-}" "${CHOCO_ROLE_MODELS:-}"
    printf '%s\n' "$END"
} > "$TMP/block"

# `gh pr list --state open` rather than `gh pr view`: branch resolution in
# `gh pr view` matches a *closed or merged* PR too. That would make the
# probe succeed on a branch whose PR a human already merged, skip the
# create, and hand every later stage a dead PR number to poll. Reachable by
# the ordinary route: escalate, merge by hand, `choco task send`,
# `revising`. Both list calls need the scope, the read-back as much as the
# probe.
N=$(gh pr list --head "$BRANCH" --state open --json number -q '.[0].number // empty')

# The create is conditional (#78): `gh pr create` fails outright when a PR
# for the branch already exists, which every lap after the first hits —
# `revising` pushes more commits to the same branch and comes back through
# here. A second lap refreshes the open PR's description instead of taking
# the `error` edge into `escalate_to_human`. The title is never refreshed.
#
# Never `--fill*` (it names the PR after the branch) and never a comment:
# one posted under the daemon's identity would be read by
# `awaiting_human_review` as the owner's own `/approve`/`/request-changes`.
if [ -z "$N" ]; then
    gh pr create --title="$T" --body-file "$TMP/block" >/dev/null
else
    gh pr view "$N" --json body -q .body > "$TMP/current"
    if splice "$TMP/current" "$TMP/block" > "$TMP/new"; then
        gh pr edit "$N" --body-file "$TMP/new" >/dev/null
    else
        note "PR #$N's description doesn't have exactly one choco block (begin, then end), e.g. a hand-written or pre-#101 body; left unchanged"
    fi
fi

if [ -s "$TMP/notes" ]; then
    cat "$TMP/notes" >&2
fi

# `gh pr create` has no `--json` flag, so the PR's number and url can only
# be read back with a separate list. The trailing `grep .` is the
# fail-closed half: an empty read-back would otherwise hand `capture: json`
# a bare `null`, and the failure would surface much later as a template
# error on `{{ stages.open_pr.number }}`. Failing here takes the `error`
# edge instead.
gh pr list --head "$BRANCH" --state open --json number,url -q '.[0] // empty' | grep .
