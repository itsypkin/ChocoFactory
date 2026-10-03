---
name: run-choco-task
description: Drive a coding task through a running ChocoFactory daemon end to end — prepare the daemon and a base checkout, write the task spec, create and watch the task, review its PR and cast the verdict, and recover a stuck, escalated or interrupted task. Use when asked to have choco implement an issue, or to run, watch, review or rescue a choco task.
---

# Run a choco task

You are the operator. Choco's agents write and review the code; you decide
what they're asked for, whether the result is good, and what happens when
they get stuck. Every stage runs the real `claude` CLI, so every lap costs
real money: read [Cost and safety](#cost-and-safety) before starting.

This skill is the judgement. The mechanics are in the README ("Running the
daemon", "Reviewing a `coding-task` PR", "Stuck tasks", "Watching a task")
and in `choco <command> --help`; check them there, because flags drift.

The workflow you will almost always run is the built-in `coding-task`:

```
coding → internal_review → open_pr → checks_polling → awaiting_human_review → done
```

Every way back lands on `revising`, which returns to `internal_review`:

- `internal_review` requests changes;
- `checks_polling` sees a failed check;
- you post `/request-changes`;
- a human resumes the task from `escalate_to_human` with `choco task send`.

A task parks at `escalate_to_human` after a 4th rejection by
`internal_review` or by you, after 6 hours without your verdict, or when
`open_pr` fails.

## 1. Prepare

**Check the daemon you are about to use.** `choco server status` shows its
version, open tasks and in-flight work. Built-in workflows and their
prompts come from the daemon *binary* (#129) and are written out when it
starts: a change merged to `workflows/` reaches no task until the daemon is
rebuilt **and restarted**, or updated with `choco update`. A rebuild alone
changes nothing for the running daemon.
`choco task status` shows which workflow a task runs (`builtin:<name>@<version>`
or a file path with a hash), and says when the built-in changed since the
task started. Before blaming a prompt for a run's behaviour, check that line
and the daemon's build.

- Don't stop or restart the daemon while an agent turn or shell step runs.
  `stop`/`restart` refuse (exit 3) unless `--force`, which marks those
  tasks `stuck`. Tasks waiting on a poll or a human survive a restart.
- Started with `choco server start`, it logs to
  `~/.config/chocofactory/logs/chocofactoryd.log` (a daemon started by hand
  logs to stderr). Watch the log when something looks wrong: kills, `stuck`
  marks and nudges show up there first.

**Point the task at a dedicated base checkout.** A task's worktree is forked
from its `--repo` checkout's HEAD at creation (default: the project's
`repo_path`). Use a detached checkout of `origin/main` that nothing else
works in, and move it to the latest `origin/main` before each task. Never use
a checkout another session is switching branches in. The task inherits
whatever that checkout's HEAD is.

**A repo can override the built-ins.** If the *project's* `repo_path` (not
the task's `--repo` checkout) has `.chocofactory/workflows/<name>.yaml`, that
file wins over the built-in of that name.
To try an unmerged workflow change on one task, pass
`--workflow <checkout>/workflows/coding-task.yaml` instead of editing
anything global.

**Keep the machine awake for the whole run.** On a sleeping laptop, agent
turns stall for hours, and a poll's wall-clock deadline keeps running: a
6-hour review window can expire overnight and park the task at
`escalate_to_human`. Something like `caffeinate -dims` in a spare terminal
is enough.

## 2. Write the spec

The `--prompt` you pass is the whole brief. It is rendered into the coder's
turn, into the reviewer's, and into every `revising` lap. Write it as a
technical spec, not a sentence: the problem grounded in real symptoms, what
to build, the design decisions with their reasons, the required tests, done
criteria, and an explicit out-of-scope list.

- **Decide the design before you write it.** A spec that leaves a real
  choice open gets one invented, and the reviewer then checks the code
  against the invention. For a contested design, get it decided first
  (someone writes the design, a person approves it), then paste the
  decision in.
- **Say the test list is a floor, not the bar.** Reviewers read a detailed
  spec as a checklist; without this line, gaps the spec didn't list get
  approved.
- **Name what not to build.** Agents gold-plate, and the reviewer then
  defends whatever they added.
- **Inline everything the task needs.** The task's worktree only has what
  is committed. A design note in a gitignored folder (such as `.omc/`) is
  invisible to it.
- **Check for collisions against `main`:** migration numbers above all,
  then file names and flags. Two tasks that both take the next number
  conflict, and the second PR's CI may not run at all.
- **The title decides what the PR closes.** A title ending in `(#N)` makes
  the PR say `Closes #N`; any other `#N` in it gives `Refs #N`. For one part
  of a multi-part issue, don't end the title with `(#N)`: merging would close
  the whole issue. Commit messages can close issues too (see
  [Review and vote](#4-review-the-pr-and-vote)).

## 3. Create and watch

```bash
choco task create --project <p> --workflow coding-task \
  --title "<what it does> (#<n>)" --repo "$BASE_CHECKOUT" \
  --prompt "$(cat spec.md)"
```

- Per-task overrides beat editing shared config: `--role-model <role>=<model>`,
  `--role-system-prompt-file <role>=<path>`. For a task that already exists,
  use `choco task reconfigure`, which takes effect on the next turn.
- Wait with `choco task status <id> --until stage:awaiting_human_review
  --timeout 2h`, or follow along with `--live`. The exit code says how it
  ended: 0 reached, 3 stuck, 4 cancelled, 5 timed out, 6 closed early.
  `--until` waits for one target only. A task that parks at
  `escalate_to_human` stays `open`, so an `--until` for another stage just
  runs to its `--timeout`. For long waits, prefer `--live` or a short
  timeout.
- `choco task events` is **oldest first**, 100 per page by default (500 at
  most with `--limit`), and a real task has hundreds to thousands of events.
  `choco task events | tail` shows the task's first minutes and looks stale.
  To read the newest events, run
  `.claude/skills/run-choco-task/scripts/tail-events.sh <id> [n]`, which
  follows `next_token` to the end.

What each stage is doing while you watch:

- **coding / revising.** The coder's turn ends when it calls
  `report_outcome`, not when it stops printing. A turn that goes quiet
  without reporting is nudged, then closed.
- **internal_review.** The reviewer routes the task on its own verdict. Its
  loop guard counts every rejection: the 4th sends the task to
  `escalate_to_human`, and escalating starts the count over.
- **open_pr.** Pushes the branch and opens or refreshes the PR. The title
  comes from the task title; the body is the coder's own description plus
  the internal reviewer's report. A closing keyword in the *body* is
  defused, but commit messages are not: the coder is told not to write
  `Closes #N` in one, and that is all.
- **checks_polling.** Polls for 5 minutes. It goes green only if every check
  reports `SUCCESS`, and red on a `FAILURE` or `ERROR` state (that starts a
  paid `revising` lap). Everything else times out into
  `awaiting_human_review` exactly as if CI had passed: no checks at all,
  skipped, cancelled or timed-out checks, and slow CI. Check `gh pr checks
  <n>` and whether the PR merges into `main` yourself.
- **awaiting_human_review.** Polls the PR's comments every minute for your
  verdict, for up to 6 hours.

## 4. Review the PR and vote

Choco's internal approval is not your review. Read the diff yourself, or have
an independent reviewer do it, and look for what agents systematically miss:

- **Does the branch still merge into `main`?** GitHub runs no `pull_request`
  workflows on a conflicting PR, so its checks stay empty.
- **Tests for the new branches**, not only the ones the spec listed. Break
  the main fix in a scratch copy and confirm a test fails.
- **Claims are not evidence.** Code comments ("deliberately untested"),
  commit messages and "addressed in <sha>" replies are claims to verify.
- **Numbered artifacts** (migrations above all) against what has landed on
  `main` since the task started.
- **New messages and states**: read each one with the values its own path
  passes, and check every new state has a way out.
- This repo's recurring findings: non-atomic read-then-write on shared
  state, and swallowed errors.

**The verdict is a PR comment**, not a GitHub review: `/approve` or
`/request-changes` alone on its own line. The README has the full rules. In
practice:

- Only comments newer than the head commit count. Editing an earlier comment
  to add the marker counts too.
- A marker inside a fenced code block still votes. When you quote the
  convention, indent it or break it up.
- Only `OWNER`, `MEMBER` and `COLLABORATOR` accounts vote, and `[bot]`
  accounts never do. Anything commenting under your account, including an
  agent, votes as you.
- Prose doesn't retract a verdict. To change your mind, post the other
  marker.

**Batch your findings into one `/request-changes`.** Each one costs a coder
lap and a review lap. Prefer approve-and-file-a-follow-up for minor points.

**Known bug (#138): after `/request-changes`, the coder often ignores your
comment** and works from the internal reviewer's old summary instead. After
the lap, check that each of your items maps to a commit. If they don't,
don't vote again (see [Recover](#5-recover)).

`/approve` moves the task to `done`. **Merging the PR is still your job.**
Before you merge, check what it will close in two places:

- `gh pr view <n> --json closingIssuesReferences` shows what the PR body
  closes.
- The commit messages are not covered by that check, and each one lands on
  `main` with a merge commit and closes what it names. Check the pushed
  branch (the PR's `headRefName`), since the task's worktree is gone once it
  is `done`:

  ```bash
  git fetch origin
  git log --format=%B origin/main..origin/<head-branch> \
    | grep -inE '(close[sd]?|fix(e[sd])?|resolve[sd]?):? +(([a-z0-9_.-]+/[a-z0-9_.-]+)?#[0-9]+|https?://github\.com/[^ ]+/issues/[0-9]+)'
  ```

## 5. Recover

**`stuck`** means the engine gave up. `choco task status` shows the reason,
and `choco task list --status stuck` finds every stuck task.

- `choco task retry <id>` re-enters the current stage. A turn that was cut
  off from outside (a usage limit, a daemon stop, the idle reaper) **resumes
  its session**, with the work already in the worktree. A turn that failed
  on its own (`no_report`, a crash) starts fresh. Use `--resume` or
  `--fresh` to force either.
- After a usage limit, wait for the reset time shown in the error event,
  then retry.
- A turn that never called `report_outcome` was nudged and then closed
  (`no_report`): there is no outcome to route on, so read its last events
  before retrying. A process that outlived its reported turn was killed
  (`lingered`): the task parks because something it started may still have
  been writing to the worktree, so check `git status` there first.

**`escalate_to_human`** happens after a 4th rejection, a 6-hour review window
with no verdict, or an `open_pr` failure. `/approve` does nothing here; only
`choco task send <id> --text "<note>"` moves it on, into `revising`.

- If the PR is already good, merge it and then `choco task cancel` the task.
  Don't send a note after merging by hand: the next `open_pr` would open a
  fresh PR.
- **The reliable way around #138** is the note: it is templated into the
  coder's prompt verbatim. No command moves a task from
  `awaiting_human_review` to `escalate_to_human`, though. After a
  `/request-changes` the coder ignored, don't vote again, since each vote
  buys another ignored coder lap and review lap. Leave the PR without a
  verdict until the 6-hour window parks the task (keep the machine awake),
  then send your items with `choco task send <id> --text "..."`. It is
  faster to cancel the task and create a new one with your items in the
  spec. A new task forks from its `--repo` checkout and redoes the work from
  scratch. To build on the old work, point `--repo` at a checkout of the old
  task's branch. Close the old PR either way.
- The internal reviewer is told to re-check a human's PR comments on its
  next round, so it sometimes catches an ignored item. Treat that as a bonus,
  not a workaround: #138's own cases came back to human review with the
  items still undone.

**`choco task cancel <id>`** is final. It kills the task's agents, marks it
cancelled and removes its worktree, so uncommitted work there is lost:
check `git -C <worktree> status` and the unpushed commits first. The branch
is left behind (#102); delete it yourself once it's merged.

**After anything unusual, look for strays:** `pgrep -f "claude --print"`.
Your own headless sessions match too, so compare each match's working
directory with the task's worktree.

## Cost and safety

- **Every stage runs the real `claude` CLI.** Don't create a task to try
  something out; use the test suite, or a daemon pointed at `mock-claude`
  through `CHOCOFACTORY_CLAUDE_BINARY` in an isolated `HOME`. Never present a
  mock run as evidence about the real CLI.
- **A repo's `.chocofactory/workflows/` and any `--workflow` file can run
  shell commands as you.** Pointing choco at a repo trusts its workflows.
- **Cancel a task that is going round in circles** rather than letting it
  spend a coder lap per round.
- `choco update` and `choco server stop` refuse while work is in flight. Use
  `--force` only if you are prepared to retry the tasks it marks stuck.

## Known gaps worth knowing

- **#138:** the coder ignores a human's `/request-changes` comment. Work
  around it as described in [Recover](#5-recover).
- **#102:** finished and cancelled tasks leave their branches behind.
- **#115:** agents sometimes call wait tools (`ScheduleWakeup`, `Monitor`)
  that never fire under `--print`, then sit idle until the daemon nudges
  them.
- **#121:** the coder's check of which PR comments are new compares local
  time with UTC.
