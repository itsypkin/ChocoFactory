# The built-in coding workflows

ChocoFactory ships two coding workflows: `coding-task`, which takes a spec to a reviewed pull request, and `coding-task-planned`, which checks the spec first. This page describes their stages, how to give a verdict on a PR, and the limits that stop them looping. Back to the [README](../README.md).

## The stages of `coding-task`

| Stage | What happens |
|---|---|
| `coding` | The `coder` agent implements the spec in the task's own git worktree. If the spec's rules leave it no way forward (for example, a pinned dependency that doesn't build and a spec that forbids substituting another), it can instead stop at `blocker`. |
| `internal_review` | A separate, read-only `reviewer` agent reviews the work against the spec. It approves, or sends the task to `revising`. |
| `revising` | The coder fixes what a review, CI or you asked for, then goes back to `internal_review`, or stops at `blocker`. |
| `open_pr` | A script pushes the branch `task/<id>` and opens the pull request. |
| `checks_polling` | Polls the PR's CI checks. Green goes on; red goes to `revising` as soon as a check has failed. Optionally (off by default) the failed GitHub Actions jobs are re-run once for the PR's current head first; see [CI re-run](#ci-re-run-of-failed-jobs-opt-in). |
| `awaiting_human_review` | Waits for your verdict on the PR (see below). |
| `base_conflict` | Reached when the PR stops merging cleanly into its base branch while it waits. It does nothing but count these rounds, then sends the task to `revising` to rebase. |
| `done` | The task is finished. |
| `escalate_to_human` | A parking stage: the task waits here for a human when a limit is hit (see [Escalation limits](#escalation-limits-and-ci-polling)). |
| `blocker` | A parking stage: the coder reported a blocker it must not work around, from `coding` or `revising`. The task waits, with no time limit, for your answer (`choco task send <id> --text "..."`). The answer goes to `revising`, and the internal reviewer sees it too and treats it as your decision. It resets no loop count. |

Every way back (a rejected internal review, red CI, a `/request-changes` from
you, a resumed escalation, an answered blocker, a PR that started conflicting with its base) goes through `revising`. The `coder` runs on
`claude-sonnet-5-5` and the `reviewer` on `claude-opus-5-5` by default; see
[models.md](models.md) to change that.

## Checking the spec first: `coding-task-planned`

`coding-task-planned` is `coding-task` with a spec check in front. Its first
stage, `spec_check`, has a planning agent read your `--prompt` against the
code the task starts from. It makes the spec buildable (fixing stale
references, commands that don't work here and loose test requirements) and
decides the design choices your intent implies, listing each with its reason.
It stops and asks you only when it would otherwise have to guess what you
want: the spec contradicts itself, the goal is missing or unclear, or every
way forward is irreversible, weakens security or costs far more than the
spec suggests.

From then on the coder and the internal reviewer work from the planner's
report, not from your `--prompt`. Read it with:

```
choco --json task status <id> | jq -r '.workflow_state.payload.stages.spec_check.summary'
```

A task waiting at `spec_questions` is answered with
`choco task send <id> --text "..."`. The answer goes back to the planner,
which folds it into the spec and checks again; you can tell it to decide a
question itself. The stage has no time limit. The workflow is offered
alongside `coding-task` so the two can be compared.

## Reviewing a `coding-task` PR

When a `coding-task` reaches `awaiting_human_review` it has already pushed
a branch, opened a PR and waited for CI. What it wants from you is a
verdict — and it reads that from the PR's **comments** or from a GitHub
**review** (the green *Review changes* button).

That is deliberate rather than a shortcut. `open_pr` pushes under whatever
identity the daemon inherited, so on a solo repo the PR belongs to the same
account that would review it, and GitHub refuses a formal review from a
PR's own author:

```
failed to create review: GraphQL: Review Can not request changes on your
own pull request (addPullRequestReview)
```

Commenting on your own PR is allowed, so on your own PR the verdict lives in
a comment, or in the body of a *Comment* review. Leave an ordinary PR comment
or a *Comment* review containing one of these markers, **alone on its own
line**:

| Marker             | Effect                          |
| ------------------ | ------------------------------- |
| `/approve`         | the task moves to `done`, unless the PR has merge conflicts (see below) |
| `/request-changes` | the task goes back to `revising` |

Merging the PR counts as approval too: while the task waits here, a merged
PR moves it to `done` exactly as `/approve` would, even with no marker
comment, and even if a `/request-changes` comment is also there, since
there is nothing left to revise once the work has landed. A PR closed
without merging is not a verdict; the task keeps reading markers until its
timeout.

**A PR that conflicts.** If the open PR can no longer merge into its base
branch (GitHub reports `mergeable: CONFLICTING`), the task does not wait for
you: it goes to `base_conflict`, then `revising`, where the coder rebases the
branch; it then runs through `internal_review`, `open_pr` (which pushes the
rebased branch) and `checks_polling` and comes back here. The order of
findings is: merged, then `/request-changes` (that round's coder lap rebases
too), then a conflict, then `/approve`. Comments you posted since the last
hand-off, marker or not, travel with the conflict note, and the coder
addresses them in the same lap (an approval among them is optional to act on). **An `/approve` on a conflicting PR is
set aside**: the task does not finish, the watcher event says so, and you
approve again once the rebased PR is back. While GitHub still reports
`UNKNOWN` (it computes this lazily, for a poll or two after a push) an
approval is held until the next poll. Two exceptions: an approval read in the
few seconds after the base branch moved can still see a stale `MERGEABLE`,
and an approval sent with `choco task send <id> --text /approve` is not
checked for conflicts at all (run `gh pr view <n> --json mergeable` first).
Rewriting history also marks your earlier comments as outdated on GitHub.

The rest of the comment is yours to write however you like — put the marker
on the last line and your review above it. A comment that reads "Two
findings, one worth fixing before merge." followed by your prose, and then
a final line containing only `/request-changes`, sends the coder back round.
The workflow hands the coder every qualifying comment (see the points
below), oldest first, in its prompt, and after them every qualifying review
(oldest first) with that review's inline comments; the coder doesn't have to
fetch them. Anything posted after the poll read the PR isn't in the prompt,
so the coder is told to check for it.

Reviews follow these rules:

- **State or marker.** A collaborator's `Approve` review (`APPROVED`) votes
  `/approve` and `Request changes` (`CHANGES_REQUESTED`) votes
  `/request-changes`, with no marker needed. A `Comment` review votes only
  through a marker line in its body. Requesting changes (by
  state or by marker) is tested first, so an `Approve` review with a
  `/request-changes` line requests changes, and a `Request changes` review
  with an `/approve` line does too.
- **Newer than the last hand-off.** A review counts by the time it was
  submitted, the same bound as for comments: later than the last time the task
  left the review gate (before the first hand-off, later than the head commit).
  A review submitted while the coder revises is carried into the next round. Pending (unsubmitted) and
  dismissed reviews never count.
- **Same author fence** as comments (below).
- **The newest vote wins** across comments and reviews together. If a comment
  and a review carry different verdicts at the same second, the tie resolves
  to request changes. A comment votes at the later of its creation and its
  last edit, so editing an older marker comment makes it the newest vote.
- **Inline comments go with their review.** An inline comment is handed over
  when its review qualifies and its own author passes the same author check
  (an owner, member or collaborator, and not a bot), whatever its own date,
  and never when its review does not qualify.
- **Editing is limited.** A review has no edited time, so editing its body or
  its inline comments counts only while the review is still newer than the
  last hand-off. Edits after the hand-off do not count; post a new comment or
  review.

Five things worth knowing:

- **Only comments and reviews newer than the last hand-off count.** Once a
  verdict has been handed to the coder it will not be read again, with or
  without a new commit, so there is nothing to clear between rounds. Before
  the first hand-off the bound is the head commit. A vote cast while the coder
  revises, approval included, is read when the task next reaches the gate; if
  it is the newest vote and an approval, the task goes to `done` without you
  seeing the revision (`done` merges nothing). The hand-off time is the daemon
  host's clock, so a clock far behind GitHub's can repeat a verdict.
- **Prose does not retract a verdict.** Only the markers are read, so a
  follow-up comment saying "wait, hold off" does not undo an `/approve` —
  and `/approve` moves the task to `done` within a minute. To change your
  mind, post the other marker.
- **Editing an earlier comment to add the marker works.** The check is on
  a comment's last-edited time, not the time it was first posted, so
  appending `/approve` to the review you already wrote counts.
- **The marker must be the whole line.** It is compared by equality once
  trailing spaces are stripped, so `> /approve` (GitHub's quote-reply
  prefix), `use /approve to vote` and `/approved` are all *not* verdicts.
  A typo is silently not a verdict either; the task just keeps waiting.
  One thing this does not exempt is a fenced code block — GitHub's API
  returns raw markdown, so a bare marker line inside triple backticks
  still votes. Indent it, or break it up, when you are quoting the
  convention rather than using it.
- **Only people with standing in the repo can vote.** A comment or review
  counts only if GitHub reports its author as `OWNER`, `MEMBER` or `COLLABORATOR`
  — this repo is public, so without that fence any passer-by could
  `/approve` a task to `done`, or burn a coder+reviewer lap at a time with
  `/request-changes`. Comments from `[bot]` accounts are skipped on top of
  that, so a CI reviewer is never mistaken for your verdict. Neither fence
  distinguishes *you* from an agent acting as you: anything commenting
  under your account counts as you.

**Answering from choco instead.** You can also answer without touching the
PR, with `choco task send`:

```
choco task send <id> --text /approve
choco task send <id> --text $'Two things to fix: …\n/request-changes'
```

The marker rules are the ones listed above for comments: `/approve` or
`/request-changes`, alone on its own line. A reply with no marker, or with
both, is refused and nothing is sent. This differs from a comment carrying
both, which counts as `/request-changes`. The rest of the reply goes to the
coder as the review. Nothing is posted to the PR. Use one channel at a time:
a choco `/approve` sent within a minute of a newer PR `/request-changes`,
before the task has read it, wins.


## Escalation limits and CI polling

`awaiting_human_review` backs off while it waits for a verdict: it checks the
PR every minute for the first 6 hours, every 5 minutes for the next 24 hours,
and every 30 minutes for the next 3 days. A verdict anywhere in that window
works as an early one, at most one polling interval late. If none arrives
within 102 hours (about four days) the task stops waiting and parks at
`escalate_to_human`, where `choco task send <id> --text "<note>"` resumes it into
`revising`. A reply that is only `/approve` or `/request-changes` is refused
there: the gate doesn't read markers, so it would be a note that starts a coder
lap. To watch again, run `choco task retry <id>`; the schedule starts over and
no coder lap is spent. Comments and reviews posted before the timeout are not
counted after that, except what the gate's last look at the deadline reads
(it runs the check once more before it gives up); post them again. If the PR was merged meanwhile, `retry` is also the
way on: the watcher sees the merge and the task moves to `done`. `retry` after
a `checks_polling` timeout re-watches CI the same way. A fourth `/request-changes`, after three revise rounds, parks
it the same way instead of looping; so does the 4th conflict round
(`base_conflict`) since the last escalation, which is counted separately from
your `/request-changes` rounds and in total, not in a row, and resuming from there starts the
count over. `internal_review` parks the task there on its 4th rejection in a
row (an approval starts its count over), and `checks_polling` does the same
on the 4th red CI result in a row (any other outcome starts it over). A red
result is the first finished red by default; with the opt-in re-run on, it is
one that is still red after the failed Actions jobs were re-run once for that
head, and while the re-run is pending the stage keeps polling and counts
nothing.
`checks_polling` polls every 30 seconds for up to 30 minutes and takes one last
look at CI when the time is up. If the machine slept through the deadline, it
watches up to 30 more minutes first, so a CI result that landed while it slept
is still seen (keep the machine awake all the same: a daemon restart drops that
extra time). A timeout, and
a cancelled, startup-failure or action-required check (`ci_cancelled`,
`ci_startup_failure`, `ci_action_required`), park the task at
`escalate_to_human`. Green (every check passed or was skipped) and
`no_checks` (no check reported for 3 minutes) go to `awaiting_human_review`.

## The PR body and the issue it closes

The PR body `open_pr` publishes holds the coder's description and the
internal reviewer's report. A closing keyword followed by an issue reference
in either is rewritten (`Resolves #84` becomes `Resolves issue 84`, inside
code blocks too, since GitHub doesn't say whether it skips code), so only the
lines the script builds from the task's closes list can close an issue.

`choco task create --closes N` (repeat it for several issues) decides what the
PR closes. Each number puts one `Closes #N` line at the top of the generated
block, so merging the PR closes those issues. A task created without
`--closes` closes nothing. The title is plain text: every `#N` in it, including
a trailing `(#N)`, only gets a `Refs #N` line, which links but doesn't close
(an issue that is also closed gets just its `Closes` line). With neither, the
PR body says so. The internal reviewer is told the closes list, and treats a
description that says the PR closes or fixes any other issue as a false claim.

## CI re-run of failed jobs (opt-in)

By default the first finished red CI result is final: `checks_polling` goes to
`revising` at once, even while other checks still run, and nothing is re-run.

A project can turn on a one-time re-run: before reporting red, the failed
GitHub Actions jobs are re-run once for the PR's current head (a failing check
that is not an Actions job is red at once, and a cancelled check is never
re-run). While the re-run is pending the stage keeps polling. With it on,
behaviour is exactly the earlier one.

The setting is `CI_RERUN_FAILED` in `checks_polling`'s `env:`:

- unset, empty or `"off"`: off (the default);
- `"on"`: on;
- anything else (`On`, `yes`, `true`, `1`, ...): each poll attempt fails with a
  note naming the variable, the stage makes no progress and times out to
  `escalate_to_human`. It never re-runs a job.

To turn it on, eject the workflow with `choco project init-workflows <project>`
(or open an existing copy) and set `CI_RERUN_FAILED: "on"`, quoted, in
`checks_polling`'s `env:`.

A key in the workflow always wins over the daemon's environment. A workflow
whose `checks_polling` has no `CI_RERUN_FAILED` key (a hand-written file, or a
copy ejected before this setting existed) inherits the variable from the
environment the daemon was started in, so "unset means off" holds only if the
operator did not export it at `choco server start`.

An ejected copy keeps the script it was ejected with. Copies made before this
setting existed still re-run. `init-workflows` never overwrites an existing
file: running it again over an old copy prints `existing` and keeps the old
YAML and script. To get the new default, delete the copied workflow files and
`scripts/ci-checks.sh` first, then eject again. Deleting only the script leaves
a keyless YAML running the new script, which takes the daemon's environment
as above.

A misspelled key name is not noticed: it reads as off.

A bad value shows in `choco task status` only as a `checks_polling` timeout
and `escalate_to_human`, the same as slow CI. The note is in `choco task
events <id> --json` (`stderr_tail`).
