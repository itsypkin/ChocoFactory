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

# Rewrites every GitHub issue reference that follows a closing keyword, so
# that agent text can never close an issue (#131). The rule: a PR body choco
# writes has exactly one authoritative issue line, the one built from the task
# title; no agent-written text may form "closing keyword + issue reference".
# Why: merging PR #127 closed #84, because the reviewer's report said
# "no Closes/Fixes/Resolves #84" and GitHub's parser ignores negation, slash
# lists and the <details> fold.
#
# The reference is rewritten, not the keyword and not with an invisible
# character: `Resolves #84` -> `Resolves issue 84`, `Closes o/r#3` ->
# `Closes o/r issue 3`, an issue URL -> `o/r issue N`. With no reference
# after the keyword nothing can close, whatever the parser does with spacing
# or punctuation; zero-width characters can't be verified and would poison
# copied text. Only the first reference after a keyword is rewritten (the
# only one GitHub closes), and a reference at the start of the next line is
# rewritten when a line ends with a keyword. Bare references stay links.
# Forms rewritten, all verified to close on real GitHub (2026-10-05): `#N`,
# `o/r#N`, `GH-N` (any case) -> `issue N`; a bare issue URL, an autolink
# `<issue URL>` and a markdown link `[any text](issue URL)` (the whole link,
# text included, since the link target counts and the text may say `#N`)
# -> `o/r issue N`. Verified NOT to close, so deliberately not handled:
# emphasis around the keyword or the reference (`**Fixes** #N`, `Fixes
# **#N**`, `_Fixes_ #N`) and a non-breaking space (U+00A0 or `&nbsp;`)
# between them. Don't add those.
# `GH-N` is tried before `o/r#N`, so `Fixes gh-1/repo#3` becomes `Fixes issue
# 1/repo#3`: the other order would leave `GH-1` right after the keyword.
# Reference-style links (`[x][1]` plus a definition) are not handled; unverified.
# Link text containing brackets (`[a [b]](URL)`, `[a\]b](URL)`) is not handled
# either: the text stops at the first `]`. Unverified.
# Code blocks and inline code are rewritten too, deliberately: GitHub doesn't
# document whether it skips code, agent text quotes commit messages where
# `Fixes #N` lives, and a cosmetic edit costs far less than a closed issue.
# Everything else passes byte for byte. Keywords are matched ASCII-only
# (the script runs with LC_ALL=C). Callers must not hide a failure of this
# filter behind a pipe: an awk error would publish an empty section.
neutralize_closing_refs() {
    awk '
        BEGIN {
            KW = "[^A-Za-z0-9_](closes|closed|close|fixes|fixed|fix|resolves|resolved|resolve)"
            URLRE = "^(https?://)?(www\\.)?github\\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/issues/[0-9]+"
            pending = 0
        }
        # Length of an issue URL at the start of u (0 if none); sets REPL to
        # "o/r issue N". stop is a bracket class of the characters that end
        # the optional "/", "#..." or "?..." tail.
        function urlat(u, ul, stop,    n, s, i, a, rest) {
            if (!match(ul, URLRE)) return 0
            n = RLENGTH
            s = substr(u, 1, n)
            i = index(tolower(s), "github.com/")
            split(substr(s, i + 11), a, "/")
            REPL = a[1] "/" a[2] " issue " a[4]
            rest = substr(u, n + 1)
            if (substr(rest, 1, 1) == "/") { n++; rest = substr(rest, 2) }
            if (match(rest, "^[#?][^ \t\r" stop "]*")) n += RLENGTH
            return n
        }
        # Length of the issue reference at index j of t (0 if none); sets REPL.
        function refat(t, tl, j,    u, ul, n, s, p, m, ang, rest) {
            u = substr(t, j); ul = substr(tl, j)
            if (match(u, /^#[0-9]+/)) {
                REPL = "issue " substr(u, 2, RLENGTH - 1)
                return RLENGTH
            }
            if (match(ul, /^gh-[0-9]+/)) {
                REPL = "issue " substr(u, 4, RLENGTH - 3)
                return RLENGTH
            }
            if (substr(u, 1, 1) == "<") {
                n = urlat(substr(u, 2), substr(ul, 2), ">")
                if (n > 0 && substr(u, n + 2, 1) == ">") return n + 2
                return 0
            }
            if (substr(u, 1, 1) == "[") {
                if (!match(u, /^\[[^]]*\]\(/)) return 0
                m = RLENGTH
                while (substr(u, m + 1, 1) ~ /[ \t]/) m++
                ang = (substr(u, m + 1, 1) == "<")
                if (ang) m++
                n = urlat(substr(u, m + 1), substr(ul, m + 1), ")>")
                if (n == 0) return 0
                m += n
                if (ang) {
                    if (substr(u, m + 1, 1) != ">") return 0
                    m++
                }
                rest = substr(u, m + 1)
                if (match(rest, /^[ \t]*\)/)) return m + RLENGTH
                if (match(rest, /^[ \t]+"[^"]*"[ \t]*\)/)) return m + RLENGTH
                if (match(rest, /^[ \t]+\047[^\047]*\047[ \t]*\)/)) return m + RLENGTH
                if (match(rest, /^[ \t]+\([^()]*\)[ \t]*\)/)) return m + RLENGTH
                return 0
            }
            n = urlat(u, ul, "")
            if (n > 0) return n
            if (match(u, /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+#[0-9]+/)) {
                n = RLENGTH
                s = substr(u, 1, n)
                p = index(s, "#")
                REPL = substr(s, 1, p - 1) " issue " substr(s, p + 1)
                return n
            }
            return 0
        }
        function neutralize(line,    t, tl, pos, op, out, s, e, c, j, k) {
            # t has a leading sentinel space so a keyword at the start of the
            # line has a non-word character before it.
            t = " " line; tl = tolower(t); pos = 1; op = 2; out = ""
            if (pending) {
                pending = 0
                j = 2
                while (substr(t, j, 1) ~ /[ \t]/) j++
                k = refat(t, tl, j)
                if (k > 0) { out = substr(t, op, j - op) REPL; op = j + k; pos = j + k - 1 }
            }
            while (1) {
                s = substr(tl, pos)
                if (!match(s, KW)) break
                e = pos + RSTART + RLENGTH - 2
                c = substr(tl, e + 1, 1)
                if (c ~ /[a-z0-9_]/) { pos = e; continue }
                j = e + 1
                if (substr(t, j, 1) == ":") j++
                while (substr(t, j, 1) ~ /[ \t]/) j++
                k = refat(t, tl, j)
                if (k > 0) {
                    out = out substr(t, op, j - op) REPL
                    op = j + k
                    pos = j + k - 1
                } else {
                    if (substr(t, j) ~ /^\r*$/) pending = 1
                    pos = e
                }
            }
            return out substr(t, op)
        }
        { print neutralize($0) }'
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
# first $1 bytes (cut at a character boundary) and a line saying so. An
# optional $2 is the real size of text that was cut before it got here: it
# is then reported as the total, and the truncation line is always printed.
cap() {
    cat > "$TMP/cap-in"
    total=$(wc -c < "$TMP/cap-in" | tr -d ' ')
    if [ -n "${2:-}" ]; then
        total=$2
    fi
    if [ "$total" -le "$1" ]; then
        cat "$TMP/cap-in"
        return 0
    fi
    utf8_head "$1" < "$TMP/cap-in" > "$TMP/cap-out"
    kept=$(wc -c < "$TMP/cap-out" | tr -d ' ')
    cat "$TMP/cap-out"
    printf '\n[truncated: %s of %s bytes shown]\n' "$kept" "$total"
}

# Copies the first $1 bytes of file $2 to file $3. Writes "$3.cut": the real
# size of $2 if it was longer than $1, otherwise empty. The caller hands that
# to cap, so a cut is never silent and the reported total is the real one.
bound_input() {
    head -c "$1" "$2" > "$3"
    size=$(wc -c < "$2" | tr -d ' ')
    if [ "$size" -gt "$1" ]; then
        printf '%s' "$size" > "$3.cut"
    else
        : > "$3.cut"
    fi
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

# Title: newlines, CRs and tabs become spaces, runs of spaces collapse to
# one, and the ends are trimmed.
T=$(printf '%s' "${PR_TASK_TITLE:-}" | tr '\n\r\t' '   ' | tr -s ' ' | sed 's/^ //; s/ $//')
[ -n "$T" ] || die "the task has no title; refusing to open a PR named after the branch"

# Pushed only once the title is known good, so a task that is going to
# escalate does not publish a branch first.
BRANCH=$(git rev-parse --abbrev-ref HEAD)

# When the branch already has an open PR, push with a lease on that PR's
# head commit: a rebased branch is never a fast-forward, and the lease still
# refuses if anyone else moved the branch. `--state open` for the same
# reason as the later probe (`gh pr view` matches closed and merged PRs).
# A failed lookup never falls back to a plain or forced push.
if ! HEAD_OID=$(gh pr list --head "$BRANCH" --state open --json number,headRefOid -q '.[0].headRefOid // empty' 2>"$TMP/lookup-err"); then
    printf 'choco open-pr: could not look up the open PR for %s; nothing was pushed\n' "$BRANCH" >&2
    cap 1024 < "$TMP/lookup-err" >&2
    exit 1
fi
if [ -n "$HEAD_OID" ]; then
    case "$HEAD_OID" in
    *[!0-9a-f]*) HEAD_OID_BAD=1 ;;
    *) HEAD_OID_BAD=0 ;;
    esac
    if [ "$HEAD_OID_BAD" = 1 ] || { [ "${#HEAD_OID}" -ne 40 ] && [ "${#HEAD_OID}" -ne 64 ]; }; then
        printf "choco open-pr: gh returned an unexpected head commit for %s's open PR; nothing was pushed\n" "$BRANCH" >&2
        exit 1
    fi
fi

if [ -n "$HEAD_OID" ]; then
    git push --force-with-lease="$BRANCH:$HEAD_OID" -u origin HEAD >/dev/null 2>"$TMP/push-err" && PUSHED=1 || PUSHED=0
else
    git push -u origin HEAD >/dev/null 2>"$TMP/push-err" && PUSHED=1 || PUSHED=0
fi
if [ "$PUSHED" != 1 ]; then
    if grep -qF '(stale info)' "$TMP/push-err"; then
        msg="the branch on GitHub moved since the PR's head $HEAD_OID: someone else pushed; look before resuming"
    elif grep -qF -e '(non-fast-forward)' -e '(fetch first)' "$TMP/push-err"; then
        msg="the push was rejected (non-fast-forward): the branch on GitHub has commits this branch doesn't, and it has no open PR; look at the branch before resuming"
    else
        msg="the push failed; git's output follows"
    fi
    printf 'choco open-pr: %s\n' "$msg" >&2
    cap 1024 < "$TMP/push-err" >&2
    exit 1
fi

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
        # Stepwise through files, not one pipe: a pipe's status is its last
        # command's, so a failing filter would publish an empty section.
        # The filter's cost grows with line length, so bound its input first
        # (4x the cap; the cap still decides what is published).
        strip_markers < "$DESC" > "$TMP/desc-0"
        bound_input 65536 "$TMP/desc-0" "$TMP/desc-1"
        neutralize_closing_refs < "$TMP/desc-1" > "$TMP/desc-2"
        cap 16384 "$(cat "$TMP/desc-1.cut")" < "$TMP/desc-2"
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
        printf '%s\n' "$PR_REVIEW_REPORT" | strip_markers > "$TMP/rep-0"
        bound_input 163840 "$TMP/rep-0" "$TMP/rep-1"
        neutralize_closing_refs < "$TMP/rep-1" > "$TMP/rep-2"
        cap 40960 "$(cat "$TMP/rep-1.cut")" < "$TMP/rep-2"
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
    # A Go template, not `-q .body`: jq output ends with a newline that
    # GitHub never stored, and the splice would keep it after the end marker,
    # growing the human's text by one newline per lap.
    gh pr view "$N" --json body -t '{{.body}}' > "$TMP/current"
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
