#!/bin/sh
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
    # $DIR/pr-mergeable is MERGEABLE (default; UNKNOWN for a merged PR, as
    # GitHub reports it), CONFLICTING or UNKNOWN; the fail-safe cases use
    # `null` (JSON null) and `absent` (no field).
    default=MERGEABLE
    [ "$state" = MERGED ] && default=UNKNOWN
    mergeable=$(cat "$DIR/pr-mergeable" 2>/dev/null || echo "$default")
    case "$mergeable" in
    null) field=',"mergeable":null' ;;
    absent) field='' ;;
    *) field=$(printf ',"mergeable":"%s"' "$mergeable") ;;
    esac
    printf '{"headRefOid":"0123456789abcdef","state":"%s","mergedAt":%s%s}' "$state" "$merged" "$field" | jq -r "$q"
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
