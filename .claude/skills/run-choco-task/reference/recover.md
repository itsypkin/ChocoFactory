# Recover a choco task

## Contents
- `stuck`: retry
- `escalate_to_human`: watch again or send a note
- Getting your review items done when the coder ignores them
- Cancelling a task
- Stray processes

## `stuck`: retry

`stuck` means the engine gave up. `choco task status <id>` shows the reason,
and `choco task list --status stuck` finds every stuck task.

- `choco task retry <id>` re-enters the current stage. A turn that was cut
  off from outside (a usage limit, a daemon stop, the daemon closing an idle
  session) **resumes its session**, with the work already in the worktree. A
  turn that failed on its own (`no_report`, a crash) starts fresh. Use
  `--resume` or `--fresh` to force either.
- After a usage limit, wait for the reset time shown in the error event, then
  retry. omp's limit message gives no reset time, and on a free tier the limit
  covers every model on the account. To finish on another CLI instead, run
  `choco task reconfigure <id> --role-cli <role>=claude` (and `--role-model`),
  then `choco task retry <id> --fresh`, since a session can't resume on a
  different CLI.
- `no_report`: the turn never called `report_outcome`, was nudged, and was
  closed. There is no outcome to route on, so read its last events before
  retrying.
- `lingered`: a process outlived its reported turn and was killed. Something
  it started may still have been writing to the worktree, so check
  `git status` there before retrying.
- A read-only role changed its worktree (the reason starts `read-only role
  '...' changed the worktree`): the change is still there, nothing was
  reverted. Inspect it, reset or clean it yourself, then retry. A retry that
  resumes the interrupted session keeps its original baseline, but any
  other retry (a fresh session) baselines whatever is in the worktree at
  that point, so reset it first. The check also runs when a read-only turn
  crashes or ends without reporting; the stuck reason then carries both.

## `escalate_to_human`: watch again or send a note

`/approve` does nothing here; a reply that is only `/approve` or
`/request-changes` is refused. After a review or CI timeout,
`choco task retry <id>` watches again from the start and costs no lap. After a
review timeout, comments and reviews posted before the timeout no longer count,
so post them again. If the
PR was merged meanwhile, after a review timeout `retry` lets the watcher see
the merge and move the task to `done`. `choco task send <id> --text "<note>"` moves the task on, into
`revising`.

- If the PR is already good, merge it and then `choco task cancel <id>`.
  Don't send a note after merging by hand: the next `open_pr` would open a
  fresh PR.
- After 4 red CI results in a row, name the failing check and what it says
  in the note, or fix CI by hand first.
- After a CI timeout, or a cancelled, startup-failure or action-required
  check, fix or re-run CI on GitHub. Once it is green, after a CI timeout run
  `choco task retry <id>`: it polls CI again and costs no lap. After a
  cancelled, startup-failure or action-required check, either merge the PR
  and cancel the task, or send a note saying CI is green and no code change
  is needed. The note costs a coder lap and a review lap, because
  resuming always goes through `revising`.
- The note is templated into the coder's prompt verbatim. Make it
  self-contained: list every item in full.
- On every lap where the branch has an open PR, `open_pr` force-pushes with
  a lease on the commit the task itself last pushed (recorded in the
  worktree's git dir), so a rebase goes through. If the PR's head is not
  that commit, someone else pushed: `open_pr` then pushes without force,
  which succeeds only if the task's branch already contains their commits
  and otherwise fails with "someone else pushed" and removes nothing. Look
  at the branch on GitHub, merge their commits into the task's branch
  (`git fetch`, then merge in the worktree), then `choco task send` the
  note; the next push goes through. A plain non-fast-forward rejection
  means the branch on GitHub has commits the task's branch lacks: inspect
  it the same way. Read the message from `stderr_tail` in
  `choco --json task events <id>`; the one-line `choco task events` view
  cuts it off.

## Getting your review items done when the coder ignores them

After `/request-changes`, the coder often works from the internal reviewer's
earlier summary instead of your comment. Work in this order:

1. **Let the next `internal_review` run.** It re-reads the PR's comments. If
   it rejects and names your items, they reach the coder through the
   reviewer's summary, which the coder does follow.
2. **If `internal_review` keeps rejecting and the task escalates**, paste every item in full into
   the note: `choco task send <id> --text "..."`. A note that only points at
   the PR ("same issues", "read the PR") doesn't work: the coder may never
   open the PR, even when told to.
3. **If the task comes back to `awaiting_human_review` with items still
   undone**, post a second `/request-changes`, or run `choco task send <id>`
   with text that lists the items in full and ends with a `/request-changes`
   line. That is the default: one more
   lap, with the reviewer backing it up again. The internal reviewer has
   three rejections available after each of your votes; your own 4th vote
   since the last escalation escalates.

The other options cost more:

- Cancel and create a new task with your items in the spec. It redoes the
  work from scratch unless `--repo` points at a checkout of the old task's
  branch, and you close the old PR either way.
- Leave the PR without a verdict until the review window (about 4 days) parks it,
  then send the note. No command moves a task from `awaiting_human_review` to
  `escalate_to_human`, so this is the last resort.

## Cancelling a task

`choco task cancel <id>` is final. It kills the task's agents, marks it
cancelled and removes its worktree and its local branch, pushed or not, so
uncommitted work there and unpushed commits are lost: check
`git -C <worktree> status` and the branch first. The branch tip is recorded
on the task's timeline before deletion. To keep the work, use
`choco task cancel <id> --keep`: it keeps both the worktree and the branch
for you (`choco task status <id>` shows where). A task that finishes
(`done`) deletes its local branch only if it was pushed or already merged;
the remote `task/<id>` branch stays unless the repo deletes merged head
branches.

## Stray processes

After anything unusual, look for strays: `pgrep -f "claude --print"`. Your
own headless sessions match too, so compare each match's working directory
with the task's worktree.
