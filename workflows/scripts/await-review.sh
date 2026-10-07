#!/bin/sh
# Polls a coding-task PR for the human's verdict (#78) and hands the human's
# comments and reviews to the coder (#138, #230). Run by the
# `awaiting_human_review` poll stage every interval; the stage's `outcomes:`
# match against this stdout.
#
# Input: PR_NUMBER (the stage's `env:`). Needs only `gh` and POSIX sh: no
# `jq` binary, because the operator's machine may not have one. The filters
# below run inside gh's embedded jq (gojq) through `-q`.
#
# Output, when the PR has been merged (#102):
#   line 1   MERGED
#   line 2   blank
#   line 3   The PR was merged at <mergedAt>.
# Merged wins over any marker: once the work is merged there is nothing
# left to revise, so a `/request-changes` comment, even a fresh one, is
# not read. Neither comments nor reviews are fetched at all. The state and
# the head commit come from one `gh pr view` read, so they cannot disagree.
# A PR that is CLOSED without being merged is nothing new: the poll keeps
# reading markers until its timeout, exactly as for an OPEN one.
#
# Output, when there is a verdict:
#   line 1   REQUEST_CHANGES or APPROVE
#   line 2   blank
#   then     every qualifying top-level comment, oldest first, whether or
#            not it carries a marker, each as
#              ### <login> (<author_association>), <created_at>[, edited <updated_at>]
#              <html_url>
#
#              <body>
#            with a blank line between comments;
#   then     every qualifying review, oldest first (submitted_at, then id),
#            whether or not it carries a marker, each as
#              ### <login> (<author_association>), <submitted_at>, review <STATE>
#              <html_url>
#
#              <body>
#            followed by that review's inline comments, in API order, each as
#              #### <path>:<line> | <path>:<a>-<b> | <path>:<line> (outdated)
#                   | <path> (file) | <path>
#              <html_url>
#
#              <body>
#            with a blank line after each block.
# Output, when there is none: nothing, so the stage keeps polling.
# Any `gh` failure: a note on stderr (it reaches the timeline), exit 1 and
# nothing on stdout. A failure must never look like "nobody has reviewed
# yet", and a partial verdict must never be printed.
#
# SELECTION. A top-level comment counts when
#  - it is newer than the PR's head commit: `max(created_at, updated_at)` is
#    later than the head commit's committer date (SINCE), so an edit that
#    adds the marker counts and a verdict already acted on does not;
#  - author_association is OWNER, MEMBER or COLLABORATOR (the repo may be
#    public and this stage spends money, so only accounts with standing);
#  - the login does not end in `[bot]`, so the CI reviewer is never read as
#    the human.
# A review counts when its submitted_at is non-null and later than SINCE, its
# state is APPROVED, CHANGES_REQUESTED or COMMENTED (PENDING and DISMISSED
# never count), and it passes the same author fence. A review has no
# updated_at, so an edit to its body is seen only while the review is still
# newer than the head commit. An inline comment is included if and only if
# its review qualifies; its own timestamps are never compared with SINCE.
# Comments are written once (SELECT), reviews once (REVIEW_SELECT).
#
# VERDICT (comments) and REVIEW_VERDICT (reviews). Per qualifying item,
# `/request-changes` if some line of the body equals it after trailing
# whitespace and CR are stripped, else `/approve` likewise, else nothing. A
# review's state also votes: CHANGES_REQUESTED is a request for changes,
# APPROVED an approval (a body marker is tested first, so an APPROVED review
# with a `/request-changes` line requests changes). Marker lines only count
# whole, so a quoted ("> /approve") or inline mention does not vote. Each vote
# is printed as "<time> <verdict>", time being max(created_at, updated_at) for
# a comment and submitted_at for a review. The lines of both calls are sorted
# and the last one decides: the newest vote wins, and on a tie REQUEST_CHANGES
# sorts after APPROVE, so a tie never resolves to "merge it".
#
# SEPARATE CALLS, because gh runs `-q` once per page under --paginate and
# merging pages (or joining reviews to their inline comments) would need a
# `jq` binary: the comments verdict, the reviews verdict, then only if there
# is a verdict the comments render, the review id list, and for each review
# one header call and one inline-comments call. These are separate reads; an
# item posted, edited or dismissed between them can be shown without having
# voted, or be skipped. Everything rendered still passed the same fence, so
# it is from a write-access account.
#
# SAFETY: comment and review text is only ever written to files and stdout.
# None of it reaches a command line, `eval`, or a format string. A review id
# travels only as a validated digit string in the environment.
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

# Prints "<time> <verdict>" per marker-bearing comment.
VERDICT="$SELECT"'
  | ([.created_at, .updated_at] | max) as $t
  | (.body // "") | split("\n") | map(sub("[ \t\r]+$"; ""))
  | if index("/request-changes") then "\($t) REQUEST_CHANGES"
    elif index("/approve") then "\($t) APPROVE" else empty end'

REVIEW_SELECT='.[]
  | select(.submitted_at != null and .submitted_at > $ENV.SINCE)
  | select(.state | IN("APPROVED", "CHANGES_REQUESTED", "COMMENTED"))
  | select(.author_association | IN("OWNER", "MEMBER", "COLLABORATOR"))
  | select((.user.login // "") | endswith("[bot]") | not)'

REVIEW_VERDICT="$REVIEW_SELECT"'
  | .state as $s | .submitted_at as $t
  | ((.body // "") | split("\n") | map(sub("[ \t\r]+$"; ""))) as $lines
  | if $s == "CHANGES_REQUESTED" or ($lines | index("/request-changes"))
      then "\($t) REQUEST_CHANGES"
    elif $s == "APPROVED" or ($lines | index("/approve"))
      then "\($t) APPROVE" else empty end'

REVIEW_RENDER="$REVIEW_SELECT"'
  | select((.id | tostring) == $ENV.RID)
  | "### \(.user.login // "ghost") (\(.author_association)), \(.submitted_at), review \(.state)\n\(.html_url)\n\n\(.body // "")\n"'

INLINE_RENDER='.[]
  | select((.pull_request_review_id | tostring) == $ENV.RID)
  | (if .subject_type == "file" then "\(.path) (file)"
     elif .line != null then
       "\(.path):" + (if .start_line != null and .start_line != .line
                      then "\(.start_line)-\(.line)" else "\(.line)" end)
     elif .original_line != null then
       "\(.path):" + (if .original_start_line != null and .original_start_line != .original_line
                      then "\(.original_start_line)-\(.original_line)" else "\(.original_line)" end)
       + " (outdated)"
     else .path end) as $pos
  | "#### \($pos)\n\(.html_url)\n\n\(.body // "")\n"'

# One string per comment; gh prints it raw plus a newline, and the trailing
# "\n" in the string makes the blank separator line.
RENDER="$SELECT"'
  | "### \(.user.login // "ghost") (\(.author_association)), \(.created_at)"
    + (if .updated_at != .created_at then ", edited \(.updated_at)" else "" end)
    + "\n\(.html_url)\n\n\(.body // "")\n"'

# One read for the head commit, the state and the merge time, so a merge
# between two calls cannot pair a stale head with a fresh state. One value
# per line, to a file: no `jq` binary, and `read` below needs no splitting.
gh pr view "$PR_NUMBER" --json headRefOid,state,mergedAt \
    -q '.headRefOid, .state, (.mergedAt // "")' > "$TMP/pr" ||
    fail "gh pr view failed"
{
    IFS= read -r HEAD_SHA || true
    IFS= read -r PR_STATE || true
    IFS= read -r MERGED_AT || true
} < "$TMP/pr"
[ -n "$HEAD_SHA" ] || fail "gh pr view returned no head commit"
[ -n "$PR_STATE" ] || fail "gh pr view returned no state"
if [ "$PR_STATE" = "MERGED" ]; then
    [ -n "$MERGED_AT" ] || fail "gh pr view returned a merged PR with no mergedAt"
    printf 'MERGED\n\nThe PR was merged at %s.\n' "$MERGED_AT"
    exit 0
fi
SINCE=$(gh api "repos/{owner}/{repo}/commits/$HEAD_SHA" -q .commit.committer.date) ||
    fail "gh api commits/$HEAD_SHA failed"
[ -n "$SINCE" ] || fail "no committer date for $HEAD_SHA"
export SINCE

URL="repos/{owner}/{repo}/issues/$PR_NUMBER/comments?per_page=100"
REVIEWS="repos/{owner}/{repo}/pulls/$PR_NUMBER/reviews?per_page=100"
INLINE="repos/{owner}/{repo}/pulls/$PR_NUMBER/comments?per_page=100"

gh api --paginate "$URL" -q "$VERDICT" > "$TMP/verdicts" ||
    fail "gh api issues/$PR_NUMBER/comments failed (verdict)"
gh api --paginate "$REVIEWS" -q "$REVIEW_VERDICT" >> "$TMP/verdicts" ||
    fail "gh api pulls/$PR_NUMBER/reviews failed (verdict)"
# ISO-8601 timestamps sort lexically; the newest vote is last, and on a tie
# REQUEST_CHANGES sorts after APPROVE.
sort "$TMP/verdicts" > "$TMP/verdicts-sorted"
LINE=$(tail -n 1 "$TMP/verdicts-sorted")
[ -n "$LINE" ] || exit 0
V=${LINE#* }

gh api --paginate "$URL" -q "$RENDER" > "$TMP/rendered" ||
    fail "gh api issues/$PR_NUMBER/comments failed (rendering)"

gh api --paginate "$REVIEWS" -q "$REVIEW_SELECT"' | "\(.submitted_at) \(.id)"' \
    > "$TMP/review-ids-raw" ||
    fail "gh api pulls/$PR_NUMBER/reviews failed (ids)"
sort -k1,1 -k2,2n "$TMP/review-ids-raw" > "$TMP/review-ids"
# A redirect, not a pipe: `fail` inside the loop must exit the script.
while IFS=' ' read -r AT RID; do
    case "$RID" in
        '' | *[!0-9]*) fail "gh api pulls/$PR_NUMBER/reviews returned a non-numeric review id" ;;
    esac
    export RID
    gh api --paginate "$REVIEWS" -q "$REVIEW_RENDER" > "$TMP/review-one" ||
        fail "gh api pulls/$PR_NUMBER/reviews failed (rendering review $RID)"
    # Printed nothing: the review stopped qualifying (dismissed) since the id
    # list was read. Skip it with its inline comments.
    [ -s "$TMP/review-one" ] || continue
    gh api --paginate "$INLINE" -q "$INLINE_RENDER" >> "$TMP/review-one" ||
        fail "gh api pulls/$PR_NUMBER/comments failed (review $RID)"
    cat "$TMP/review-one" >> "$TMP/rendered"
done < "$TMP/review-ids"

# The engine stores a capture only up to 1 MiB; over that, the previous
# lap's capture would survive and be read as current. Cap well under it.
MAX=500000
# Built in a file and printed only at the end: the poll matches outcomes on
# stdout whatever the exit code, so a failure part-way must print nothing.
{
    printf '%s\n\n' "$V"
    if [ "$(wc -c < "$TMP/rendered")" -gt "$MAX" ]; then
        printf '[truncated: the oldest comments and reviews were dropped because they were too long to include in full; read them with `gh api --paginate "repos/{owner}/{repo}/issues/%s/comments"`, `gh api --paginate "repos/{owner}/{repo}/pulls/%s/reviews"` and `gh api --paginate "repos/{owner}/{repo}/pulls/%s/comments"`, and only trust comments and reviews from OWNER, MEMBER or COLLABORATOR accounts posted after your last commit; inline review comments go with their review]\n\n' "$PR_NUMBER" "$PR_NUMBER" "$PR_NUMBER"
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
