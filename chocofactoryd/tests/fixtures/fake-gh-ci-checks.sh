#!/bin/sh
# $DIR/states: one check state per line. Switches (files in $DIR):
# fail-view, fail-checks, empty-checks, empty-head, count (overrides the count),
# fail-rerun (`run rerun` exits 1). $DIR/links: one link per check, in states
# order; an empty line is "", the line <none> leaves the field out.
DIR="$GH_FAKE_DIR"
echo "$*" >> "$DIR/calls"
q=""
prev=""
for a in "$@"; do
    if [ "$prev" = "-q" ]; then q=$a; fi
    prev=$a
done
if [ "$1" = run ] && [ "$2" = rerun ]; then
    [ -e "$DIR/fail-rerun" ] && { echo "fake gh: run rerun failed" >&2; exit 1; }
    echo "fake gh: requested rerun"
    exit 0
fi
[ "$1" = pr ] || { echo "fake gh: unhandled: $*" >&2; exit 1; }
case "$2" in
view)
    [ -e "$DIR/fail-view" ] && { echo "fake gh: pr view failed" >&2; exit 1; }
    head=0123456789abcdef
    [ -e "$DIR/empty-head" ] && head=""
    rollup=$(jq -R -s -c 'split("\n") | map(select(. != "")) | map({state: .})' < "$DIR/states")
    json=$(printf '{"headRefOid":"%s","statusCheckRollup":%s}' "$head" "$rollup")
    if [ -e "$DIR/count" ]; then
        json=$(printf '%s' "$json" | jq -c ".statusCheckRollup = $(cat "$DIR/count")")
        # A count that is not a list: `length` of it is the value itself for
        # numbers, and the string's length otherwise, so emit it directly.
        printf '%s\n%s\n' "$head" "$(cat "$DIR/count" | tr -d '"')"
        exit 0
    fi
    printf '%s' "$json" | jq -r "$q"
    ;;
checks)
    [ -e "$DIR/fail-checks" ] && { echo "fake gh: pr checks failed" >&2; exit 1; }
    [ -e "$DIR/empty-checks" ] && exit 0
    lf="$DIR/links"
    if [ ! -e "$lf" ]; then
        lf="$DIR/links.default"
        awk '{ printf "https://github.com/o/r/actions/runs/100/job/%d\n", NR - 1 }' "$DIR/states" > "$lf"
    fi
    jq -R -s -c --rawfile links "$lf" '($links | split("\n")) as $l | split("\n") | map(select(. != "")) | to_entries | map({name: ("job-" + (.key | tostring)), state: .value} + (if $l[.key] == "<none>" then {} else {link: $l[.key]} end))' < "$DIR/states" | jq -r "$q"
    exit 0
    ;;
*) echo "fake gh: unhandled: $*" >&2; exit 1 ;;
esac
