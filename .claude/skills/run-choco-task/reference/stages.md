# What each `coding-task` stage does

Read this while watching a task, when its behaviour surprises you.

- **spec_check** (`coding-task-planned` only). A planning agent checks
  the spec against the task's starting commit and the remote's default
  branch, fixes what it finds, and decides the design choices your
  intent implies. It asks only when it would have to guess what you
  want.
  Its report replaces your `--prompt` for every later stage. The planner
  can't use the file-editing tools, and a turn that changes HEAD or
  `git status` anyway parks the task as `stuck`, with the change left in
  place for you to inspect. The check covers HEAD, the branch, `git status`
  and file contents; it doesn't cover ignored paths (`target/`, `.omc/`) or
  anything inside `.git` (refs, config, hooks). It also runs when the turn
  crashes or ends without reporting.
- **spec_questions** (`coding-task-planned` only). Waits, with no time
  limit, for `choco task send`. The answer goes back to `spec_check`,
  never straight to the coder.
- **coding / revising.** The coder's turn ends when it calls
  `report_outcome`, not when it stops printing. A turn that goes quiet
  without reporting is nudged, then closed, unless a background job it started
  is still running (Claude Code only): then it is not nudged, and is closed
  once its waits on jobs add up to 60 minutes in total for the turn (a later
  wait gets only what is left, never a fresh 60 minutes). In `revising`, the coder decides
  which PR comments are new by comparing their times with its last commit,
  and it can get that wrong across time zones, so a comment of yours may be
  treated as already handled.
- **internal_review.** The reviewer is told the task's closes list and treats
  a description that claims the PR closes any other issue as a false claim.
  The reviewer can't use the file-editing tools, and a
  turn that changes HEAD or `git status` anyway parks the task as `stuck`,
  with the change left in place for you to inspect (same coverage and gaps
  as `spec_check`: not ignored paths, not anything inside `.git`). Before it
  approves, it runs the repo's whole gate (the commands the repo's
  instruction files say a change must pass) in its own scratch copy of HEAD,
  never in the task's worktree, and gives a failing test the change doesn't
  touch one re-run on its own, and it blocks only if it fails again. It
  approves only after a gate that ran to the end. It routes the task on its
  own verdict. Its loop guard counts rejections in a row: the 4th in a
  row sends the task to `escalate_to_human`. An approval or escalating
  starts the count over, so your `/request-changes` always gets a fresh
  internal budget. It re-reads the PR's comments, so it can pick up review
  items the coder ignored.
- **open_pr.** Pushes the branch `task/<task-id>` and opens or refreshes the
  PR. The title comes from the task title; the body is the coder's own
  description plus the internal reviewer's report. The issue lines at the top
  come from the task's `--closes` list (`Closes #N` each); a `#N` in the title
  only gives `Refs #N`. A closing keyword followed
  by an issue reference in the body is rewritten so it can't close anything,
  but markdown-wrapped forms (`**Fixes** #N`, `[#N](url)`, `GH-N`) aren't
  verified. Commit messages are not rewritten at all: the coder is told not
  to write `Closes #N` in one, and that is all. The pre-merge checks in the
  skill catch both.
- **checks_polling.** Polls every 30 seconds for up to 30 minutes.
  It goes green when every check passed or was skipped. It goes red on a
  failed, errored or timed-out check, as soon as one has finished. Only if the
  workflow sets `CI_RERUN_FAILED: "on"` in `checks_polling`'s `env:` is the
  failed Actions job re-run once for this head first, and then red means still
  red after that re-run (a failing check that is not an Actions job is red at
  once). A `CI_RERUN_FAILED` value other than `on`, `off` or empty fails every
  poll and ends as a timeout (see recover.md). Red starts a paid `revising` lap (the
  4th red in a row parks the task at `escalate_to_human`). It escalates to
  `escalate_to_human` on a cancelled, startup-failure or action-required
  check (the outcome name in `choco task status` says which), and when CI
  has not finished in 30 minutes (it takes one last look at the deadline, and
  watches up to 30 more minutes if the machine slept through it, unless the
  daemon restarts meanwhile). A real failure wins over those three. A PR
  with no checks at all goes to `awaiting_human_review` as `no_checks` after
  3 minutes, so on a repo without CI, or on a conflicting PR, you review
  without CI (the gate then sends a conflicting PR to rebase). The timeline lists each check with its state.
- **awaiting_human_review.** A human gate that watches the PR's comments and reviews
  every minute for 6 hours, every 5 minutes for the next 24 hours and every
  30 minutes for the next 3 days, takes one last look, then parks at
  `escalate_to_human`. It also takes
  `choco task send <id> --text "..."` carrying `/approve` or
  `/request-changes` on a line of its own; a reply with neither or both is
  refused. A merged PR counts as approval and moves the task to `done`; a PR
  closed without merging does not. A third finding is `conflicting`: an open
  PR whose `mergeable` is `CONFLICTING` goes to `base_conflict` (a stage that
  only counts these rounds; the 4th since the last escalation parks the task)
  and then `revising` to rebase. Order: merged, `/request-changes`, conflict,
  `/approve`. An approval of a conflicting PR is set aside; an `UNKNOWN`
  `mergeable` holds an approval for one more poll.
- **escalate_to_human.** Arriving here starts all four loop counts over (internal rejections, red CI
  results, your votes, conflict rounds).
  It waits for you. After a review or CI timeout, `choco task retry <id>`
  watches again at no cost. Otherwise `choco task send <id> --text "<note>"`
  moves the task into `revising`.
