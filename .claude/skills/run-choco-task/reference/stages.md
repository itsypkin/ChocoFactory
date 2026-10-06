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
  loop guard counts every rejection: the 4th sends the task to
  `escalate_to_human`, and only escalating starts the count over. Your
  `/request-changes` doesn't reset it. It re-reads the PR's comments, so it
  can pick up review items the coder ignored.
- **open_pr.** Pushes the branch `task/<task-id>` and opens or refreshes the
  PR. The title comes from the task title; the body is the coder's own
  description plus the internal reviewer's report. A closing keyword followed
  by an issue reference in the body is rewritten so it can't close anything,
  but markdown-wrapped forms (`**Fixes** #N`, `[#N](url)`, `GH-N`) aren't
  verified. Commit messages are not rewritten at all: the coder is told not
  to write `Closes #N` in one, and that is all. The pre-merge checks in the
  skill catch both.
- **checks_polling.** Polls for 5 minutes. It goes green only if every check
  reports `SUCCESS`, and red on a `FAILURE` or `ERROR` state (that starts a
  paid `revising` lap). Everything else times out into
  `awaiting_human_review` exactly as if CI had passed: no checks at all,
  skipped, cancelled or timed-out checks, and slow CI.
- **awaiting_human_review.** Polls the PR's comments every minute for your
  verdict, for up to 6 hours, then parks at `escalate_to_human`. A merged
  PR counts as approval and moves the task to `done`; a PR closed without
  merging does not.
- **escalate_to_human.** Arriving here starts both rejection counts over.
  It waits for `choco task send <id> --text "<note>"`, which moves the task
  into `revising`.
