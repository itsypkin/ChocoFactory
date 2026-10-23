#!/bin/sh
# Ask one omp role, run by your own choco daemon, a question about a commit's
# files, and print its answer.
#
# The commit is exported with `git archive` into a temp folder outside any
# checkout, and one turn of the omp-probe workflow next to this script runs
# there with no worktree, so it sees exactly that commit's files and loads
# what your daemon gives any omp role. The folder is removed at the end; a
# task that hasn't closed by then is cancelled first.
#
# Usage: omp-probe.sh <project> <repo> <commit> <model> <question>
# Exit:  0 answered; 1 error; 2 usage; 3 answered, but the turn called a tool
#        other than report_outcome, so the answer may come from a file it read
# Env:   CHOCO           path to the choco binary (default: `choco` on PATH)
#        CHOCO_BASE_URL  read by choco itself, as usual
set -eu

usage() {
    echo "usage: omp-probe.sh <project> <repo> <commit> <model> <question>" >&2
    exit 2
}

[ "$#" -eq 5 ] || usage
project=$1 repo=$2 commit=$3 model=$4 question=$5

CHOCO=${CHOCO:-choco}
for tool in "$CHOCO" jq git tar; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "omp-probe.sh: '$tool' not found on PATH" >&2
        exit 1
    }
done
workflow=$(cd "$(dirname "$0")" && pwd)/omp-probe/omp-probe.yaml
[ -f "$workflow" ] || {
    echo "omp-probe.sh: workflow file $workflow is missing" >&2
    exit 1
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/omp-probe.XXXXXX")
task=''
closed=''
watch=''
# On every exit: stop the watch, cancel an unfinished task, and only then
# remove the folder its turn reads. The signal traps make the signals end
# the script.
cleanup() {
    # A signal during `task create` runs this trap after create has finished,
    # when its output is in the file but `task` isn't set yet.
    if [ -z "$task" ] && [ -f "$tmp/create.json" ]; then
        task=$(jq -r '.id // empty' "$tmp/create.json" 2>/dev/null) || task=''
    fi
    if [ -n "$watch" ]; then
        kill "$watch" 2>/dev/null || :
    fi
    if [ -n "$task" ] && [ -z "$closed" ]; then
        "$CHOCO" task cancel "$task" >&2 ||
            echo "omp-probe.sh: couldn't cancel task $task; cancel it yourself" >&2
    fi
    rm -rf "${tmp:?}"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir "$tmp/tree"
git -C "$repo" archive --format=tar -o "$tmp/tree.tar" "$commit"
tar -x -f "$tmp/tree.tar" -C "$tmp/tree"

prompt="Answer from your system prompt and context only. Call no tool except report_outcome, and put your whole answer in its summary.

$question"
"$CHOCO" --json task create --project "$project" --workflow "$workflow" \
    --title "omp probe of $commit" --repo "$tmp/tree" \
    --role-model "probe=$model" --prompt "$prompt" >"$tmp/create.json"
task=$(jq -r '.id // empty' "$tmp/create.json")
[ -n "$task" ] || {
    echo "omp-probe.sh: task create returned no id" >&2
    exit 1
}
echo "omp-probe.sh: task $task" >&2

# In the background, so a signal ends the script without waiting for it.
"$CHOCO" task status "$task" --until attention --timeout 10m >&2 &
watch=$!
status=0
wait "$watch" || status=$?
watch=''
case $status in
    6) closed=1 ;;
    4)
        closed=1
        echo "omp-probe.sh: task $task was cancelled" >&2
        exit 1
        ;;
    *)
        echo "omp-probe.sh: task $task didn't close (watch exit $status)" >&2
        exit 1
        ;;
esac

events=$("$CHOCO" --json task events "$task" --limit 500)
if [ -n "$(printf '%s\n' "$events" | jq -r '.next_token // empty')" ]; then
    echo "omp-probe.sh: task $task has more than 500 events; read them with choco task events" >&2
    exit 1
fi
answer=$(printf '%s\n' "$events" | jq -r '[.events[] | select(.event_type == "tool_call")
    | .payload | select((.tool // "") | endswith("report_outcome")) | .input.summary // empty] | last // empty')
[ -n "$answer" ] || {
    echo "omp-probe.sh: task $task closed without a report" >&2
    exit 1
}
printf '%s\n' "$answer"
others=$(printf '%s\n' "$events" | jq -r '[.events[] | select(.event_type == "tool_call")
    | .payload.tool // "?" | select(endswith("report_outcome") | not)] | unique | join(", ")')
if [ -n "$others" ]; then
    echo "omp-probe.sh: the turn also called: $others; the answer may come from a file it read" >&2
    exit 3
fi
