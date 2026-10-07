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
  without reporting is nudged, then closed. In `revising`, the coder decides
  which PR comments are new by comparing their times with its last commit,
  and it can get that wrong across time zones, so a comment of yours may be
  treated as already handled.
- **internal_review.** The reviewer can't use the file-editing tools, and a
  turn that changes HEAD or `git status` anyway parks the task as `stuck`,
  with the change left in place for you to inspect (same coverage and gaps
  as `spec_check`: not ignored paths, not anything inside `.git`). It routes the task on
  its own verdict. Its
  loop guard counts rejections in a row: the 4th in a row sends the task to
  `escalate_to_human`. An approval or escalating starts the count over, so
  your `/request-changes` always gets a fresh internal budget. It re-reads the PR's comments, so it
  can pick up review items the coder ignored.
- **open_pr.** Pushes the branch `task/<task-id>` and opens or refreshes the
  PR. The title comes from the task title; the body is the coder's own
  description plus the internal reviewer's report. A closing keyword followed
  by an issue reference in the body is rewritten so it can't close anything,
  but markdown-wrapped forms (`**Fixes** #N`, `[#N](url)`, `GH-N`) aren't
  verified. Commit messages are not rewritten at all: the coder is told not
  to write `Closes #N` in one, and that is all. The pre-merge checks in the
  skill catch both.
- **checks_polling.** Polls every 30 seconds for up to 30 minutes.
  It goes green when every check passed or was skipped. It goes red on a
  failed, errored or timed-out check that is still red after one automatic
  re-run of the failed Actions jobs for this head (a failing check that is not
  an Actions job is red at once). Red starts a paid `revising` lap (the
  4th red in a row parks the task at `escalate_to_human`). It escalates to
  `escalate_to_human` on a cancelled, startup-failure or action-required
  check (the outcome name in `choco task status` says which), and when CI
  has not finished in 30 minutes. A real failure wins over those three. A PR
  with no checks at all goes to `awaiting_human_review` as `no_checks` after
  3 minutes, so on a repo without CI, or on a conflicting PR, you review
  without CI. The timeline lists each check with its state.
- **awaiting_human_review.** A human gate that watches the PR's comments
  every minute for 6 hours, then parks at `escalate_to_human`. It also takes
  `choco task send <id> --text "..."` carrying `/approve` or
  `/request-changes` on a line of its own; a reply with neither or both is
  refused. A merged PR counts as approval and moves the task to `done`; a PR
  closed without merging does not.
- **escalate_to_human.** Arriving here starts all three loop counts over (internal rejections, red CI
  results, your votes).
  It waits for `choco task send <id> --text "<note>"`, which moves the task
  into `revising`.
