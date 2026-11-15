#!/bin/sh
# Tells a parked human gate (`escalate_to_human`, `blocker`) that its PR has
# been merged (#379). Run by the gate's `watch:` every interval; the gate's
# `outcomes:` match against this stdout.
#
# Input: PR_NUMBER (the gate's `env:`, from `stages.open_pr.number`), empty
# when no PR was opened yet. Needs only `gh` and POSIX sh.
#
# Output: `MERGED` and a newline when the PR's state is exactly MERGED, and
# nothing otherwise. OPEN, CLOSED without a merge, an empty or unexpected
# state all print nothing and exit 0: the gate keeps waiting for its human,
# and the watcher reads again at its next interval. This is not
# `await-review.sh`: it reads no comments, reviews or conflict state, so a
# marker or a conflict can never move a parked gate.
#
# A failed read is not "no merge". When `gh` fails, this exits with its status
# and its stderr and prints nothing on stdout, even if `gh` wrote MERGED
# before failing. The attempt is recorded as failed, the gate stays where it
# is, and the next interval reads again.
#
# No PR number: exit 0 with no output and without calling `gh`.

[ -n "${PR_NUMBER:-}" ] || exit 0

state=$(gh pr view "$PR_NUMBER" --json state -q '.state') || exit $?

if [ "$state" = "MERGED" ]; then
  printf 'MERGED\n'
fi
exit 0
