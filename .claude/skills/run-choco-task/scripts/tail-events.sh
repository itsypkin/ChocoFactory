#!/bin/sh
# Print the newest N events of a choco task, oldest of them first.
#
# `choco task events` returns the *oldest* events first, at most 500 per
# page, so a plain `| tail` shows a long task's first minutes. This follows
# `next_token` to the last page and prints the tail of the whole history.
#
# Usage: tail-events.sh <task-id> [n]     (n defaults to 20)
# Env:   CHOCO           path to the choco binary (default: `choco` on PATH)
#        CHOCO_BASE_URL  read by choco itself, as usual
set -eu

usage() {
    echo "usage: tail-events.sh <task-id> [n]" >&2
    exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
task=$1
n=${2:-20}
case $n in
    '' | *[!0-9]* | 0) echo "tail-events.sh: n must be a positive integer, got '$n'" >&2; usage ;;
esac

CHOCO=${CHOCO:-choco}
command -v "$CHOCO" >/dev/null 2>&1 || {
    echo "tail-events.sh: choco binary '$CHOCO' not found (set CHOCO to its path)" >&2
    exit 1
}
command -v jq >/dev/null 2>&1 || {
    echo "tail-events.sh: jq is required but not on PATH" >&2
    exit 1
}

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT HUP INT TERM

token=''
while :; do
    if [ -n "$token" ]; then
        page=$("$CHOCO" --json task events "$task" --limit 500 --after "$token")
    else
        page=$("$CHOCO" --json task events "$task" --limit 500)
    fi
    printf '%s\n' "$page" | jq -c '.events[]' >>"$tmp"
    next=$(printf '%s\n' "$page" | jq -r '.next_token // empty')
    [ -n "$next" ] || break
    if [ "$next" = "$token" ]; then
        echo "tail-events.sh: the daemon returned the same next_token twice; stopping" >&2
        exit 1
    fi
    token=$next
done

tail -n "$n" "$tmp" | jq -r '
    (.payload | (.text // .message // .note // .input.command // .input.file_path
                 // (if .attempt then "\(.stage // "") attempt \(.attempt), exit \(.exit_code // "?") \(.stdout_tail // "")" else null end)
                 // (if .stage then "\(.stage) \(.outcome // "")" else null end)
                 // tostring)) as $detail
    | "\(.created_at[0:19] | sub("T"; " ")) \(.event_type)  \($detail | tostring | gsub("\\s+"; " ") | .[0:160])"'
