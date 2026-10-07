---
name: run-choco-task
description: Drives a coding task through a ChocoFactory (choco) daemon end to end — update choco and check its daemon, prepare a base checkout, write the task spec, create and watch the task, review its PR and cast the verdict, and recover a stuck, escalated or interrupted task. Use when asked to have choco implement an issue, or to run, watch, review or rescue a choco task.
---

# Run a choco task

You are the operator. Choco's agents write and review the code; you decide
what they are asked for, whether the result is good, and what happens when
they get stuck. Every stage runs the real `claude` CLI, so every lap costs
real money: read [Cost and safety](#cost-and-safety) before you start.

Flags are in `choco <command> --help`; this skill is the judgement around
them.

The built-in `coding-task` workflow:

```
coding → internal_review → open_pr → checks_polling → awaiting_human_review → done
```

Every way back goes through `revising`, which returns to `internal_review`:
`internal_review` requests changes, `checks_polling` sees a failed check, you
post `/request-changes`, or someone resumes the task from `escalate_to_human`.

A task parks at `escalate_to_human` after a 4th rejection in a row by
`internal_review`, a 4th red CI result in a row from `checks_polling`, a 4th
`/request-changes` from you since the last escalation (each counted
separately), after 6 hours with no verdict from you, when CI has not
finished in 30 minutes, when a check is cancelled, failed to start or needs
an action (approval) before it can run, or when `open_pr` fails.

Copy this checklist and tick it off:

```
- [ ] 1. Prepare: choco up to date, daemon running, base checkout on the latest default branch
- [ ] 2. Write the spec
- [ ] 3. Create the task and wait for awaiting_human_review (or a parked state)
- [ ] 4. Review the PR, vote, and run the pre-merge checks
- [ ] 5. Recover if it got stuck or escalated
```

## 1. Prepare

**Update choco first.** This skill describes the latest release. Agents'
prompts ship inside the daemon, so an older daemon behaves differently from
what follows.

```bash
choco update             # installs the latest release and restarts a daemon running from the install folder
choco --version
```

- `choco: command not found`: install it as in the
  [README](https://github.com/itsypkin/ChocoFactory#install), then go on.
- `choco update` refuses while work is in flight; see
  [Cost and safety](#cost-and-safety).
- It refuses with exit 1 for a copy that wasn't installed by the install
  script; the error says what to run instead.

**First run.** Once per machine and repo, from the repo choco will work on:

```bash
claude --version              # agents run the claude CLI, logged in as you
gh auth status                # open_pr and the polls run gh as this account; it is who "you" are when voting
choco server start && choco server status
choco project list            # or: choco project create <name> --repo .
git worktree add --detach ../<name>-base origin/main      # once
BASE_CHECKOUT=$(cd ../<name>-base && pwd)                 # again in each new shell
```

**Before each task**, check the daemon and move the base checkout to the
latest default branch:

```bash
choco server status
git -C "$BASE_CHECKOUT" fetch origin && git -C "$BASE_CHECKOUT" checkout --detach origin/main
```

Replace `origin/main` with your default branch if it differs, here and in
the pre-merge check in step 4.

- **The base checkout is what the task forks from.** A task's worktree
  starts at its `--repo` checkout's HEAD (default: the project's repo). Use
  a detached checkout that nothing else works in. Never point it at a
  checkout another session is switching branches in.
- **`choco server status`** shows the daemon's version, open tasks and
  in-flight work. It warns when `choco` and the daemon differ in version, or
  the daemon binary changed on disk since it started: run
  `choco server restart` then.
- **`choco task status <id>`** shows which workflow a task runs
  (`builtin:<name>@<version>`, or a file path with a hash) and marks it
  `(built-in updated since task start)`, or `(changed since task start)` for
  a workflow file, when it changed. Check that line and the
  daemon's version before blaming a prompt for a run's behaviour.
  It ends with a `Cost & time` block: the task's cost (`≈ $…`, marked
  `(API-equivalent)` when every turn ran under a subscription login and
  `(estimated)` otherwise), tokens, wall and active time, and the same split
  by stage, role, lap and model. A figure the CLI did not report reads `no
  data`, `cost unknown` or `?`, never zero, and a partial total says so
  (`(N sessions without data)`, `(N turns without a cost)`). `choco --json task list` carries
  each task's total as `usage_total`, and the dashboard shows it in a `cost`
  column on wide terminals.
- **A project with no repo** (`REPO -` in `choco project list`) always gets
  the built-in workflows, and every `coding-task` needs `--repo`.
- **A repo can override a built-in.** If the *project's* repo has
  `.chocofactory/workflows/<name>.yaml`, that file wins over the built-in of
  that name, and choco updates no longer change it (`choco project
  init-workflows` creates exactly that folder). Customising workflows is out
  of scope here; see
  [Customising workflows](https://github.com/itsypkin/ChocoFactory#customising-workflows).
- **Don't stop or restart the daemon while an agent turn or shell step
  runs.** `stop` and `restart` refuse (see [Cost and safety](#cost-and-safety)).
  Tasks waiting on a poll or a human survive a restart.
  The daemon logs to `~/.config/chocofactory/logs/chocofactoryd.log`; kills,
  `stuck` marks and nudges show up there first.
- **Keep the machine awake for the whole run.** On a sleeping laptop agent
  turns stall, and the 6-hour review window keeps running and can park the
  task overnight. On macOS, run `caffeinate -dims` in a spare terminal. Don't
  tie it to the daemon with `-w <pid>`: the pid changes on restart.

## 2. Write the spec

The `--prompt` is the whole brief. It is rendered into the coder's turn, the
reviewer's, and every `revising` lap. Write a technical spec, not a
sentence: the problem grounded in real symptoms, what to build, the design
decisions with their reasons, the required tests, done criteria, and an
explicit out-of-scope list.

- **Decide the design before you write it.** A spec that leaves a real
  choice open gets one invented, and the reviewer then checks the code
  against the invention. For a contested design, get a person to approve it
  first, then paste the decision in.
- **Say the test list is a floor, not the bar.** Reviewers read a detailed
  spec as a checklist; without this line, gaps the spec didn't list get
  approved.
- **Name what not to build.** Agents gold-plate, and the reviewer then
  defends whatever they added.
- **Inline everything the task needs.** The task's worktree only has what is
  committed. A design note in a gitignored folder is invisible to it.
- **Repo-wide rules go in the repo's instruction files.** An agent reads the
  repo's own `CLAUDE.md` and `AGENTS.md` (the root ones at start, nested ones
  when it reads a file in that folder), but not your `~/.claude/CLAUDE.md` or
  instruction files in folders above the repo. Anything from your personal
  setup that the task needs goes in the spec.
- **Check that the design holds, not only that it builds.** For a feature
  that enforces or protects something, list every way the protected event
  can end, including a fresh retry, a resumed retry and the daemon's
  restart sweep, and say whether the protection runs on each. Mark each
  blind spot of the check (content vs status, ignored paths, a mistyped
  config key) as covered or as an accepted cost. Say which condition each
  prescribed message is true under. For a layout, list its invariants and
  require one test that renders every fixture at every size from the
  minimum up. Name one test for each fail-closed path in the done
  criteria.
- **Check for collisions against your default branch:** numbered files such
  as database migrations above all, then file names and flags. Two tasks that
  both take the next number conflict, and the second PR's CI may not run.
- **The title decides what the PR closes.** A title ending in `(#N)` makes
  the PR say `Closes #N`; any other `#N` in it gives `Refs #N`. For one part
  of a multi-part issue, don't end the title with `(#N)`: merging would close
  the whole issue.
- **To have choco check the spec before coding**, create the task with
  `--workflow coding-task-planned`. A planning agent checks the spec
  against the code the task starts from, fixes stale references and
  loose test requirements, checks that the design holds on every path,
  and decides the design choices your intent
  implies, listing each with its reason. It parks the task at
  `spec_questions` with questions for you only when it can't go on
  without guessing what you want: the spec contradicts itself, the goal
  is unclear, or the only way forward is irreversible, weakens security
  or costs far more than the spec suggests. From then on, the coder
  and the reviewer work from its report, not your `--prompt`. Read the
  report with
  `choco --json task status <id> | jq -r '.workflow_state.payload.stages.spec_check.summary'`,
  and answer with `choco task send <id> --text "<answers>"`. The planner
  folds your answers in and checks again; an answer can tell it to
  decide a question itself.

## 3. Create and watch

```bash
choco task create --project <p> --workflow coding-task \
  --title "<what it does> (#<n>)" --repo "$BASE_CHECKOUT" \
  --prompt "$(cat spec.md)"
```

- Per-task overrides beat editing shared config:
  `--role-model <role>=<model>`, `--role-system-prompt-file <role>=<path>`.
  For an existing task, `choco task reconfigure` takes effect on the next
  turn.
- **Wait for one stage** with `choco task status <id> --until
  stage:awaiting_human_review --timeout 2h`. The exit code says how it ended:

  | Exit | Meaning |
  |---|---|
  | 0 | target reached |
  | 1 | error (unknown task, daemon unreachable) |
  | 3 | became `stuck` |
  | 4 | cancelled |
  | 5 | `--timeout` elapsed |
  | 6 | closed without reaching the target |

  A task parked at `escalate_to_human` stays `open`, so an `--until` for
  another stage runs to its `--timeout`.
- **To notice every way it can stop** (ready for review, escalated, waiting for your answers, ended),
  poll the JSON in the background instead (needs `jq`). It prints the status
  and stage it stopped on, and exits 1 if `choco` or `jq` fails:

  ```bash
  id=<task-id>
  (
    while :; do
      json=$(choco --json task status "$id") || exit 1
      state=$(printf '%s\n' "$json" | jq -r '"\(.status) \(.workflow_state.stage_kind) \(.workflow_state.current_stage)"') || exit 1
      case $state in
        "open human_gate "* | stuck\ * | cancelled\ * | closed\ *)
          echo "$state"; exit 0 ;;
      esac
      sleep 60
    done
  )
  ```

  The parentheses keep `exit` from closing your shell. After you vote, the
  task stays at `awaiting_human_review` for up to a minute until the poll
  reads your comment, so wait for the stage to change first, for example
  `choco task status <id> --until stage:revising --timeout 5m`.
- **Find the task's PR** with `gh pr list --head task/<task-id>` (add
  `--state all` once it is merged or closed). `choco task status` doesn't
  print it.
- **Read the newest events** by running
  `${CLAUDE_SKILL_DIR}/scripts/tail-events.sh <task-id> [n]` (needs `jq`).
  `choco task events` is oldest first, so `choco task events | tail` shows
  the task's first minutes and looks stale.

What each stage does, and what can mislead you while it runs, is in
[reference/stages.md](reference/stages.md).

## 4. Review the PR and vote

Choco's internal approval is not your review. Read the diff yourself, or have
an independent reviewer do it, and look for what agents systematically miss:

- **Does the branch still merge into your default branch?** GitHub runs no
  `pull_request` workflows on a conflicting PR, so its checks stay empty.
- **Did CI actually pass?** Green from `checks_polling` means every check
  passed or was skipped. A task that arrives through `no_checks` means no CI
  ran on the PR (a repo without CI, or a PR GitHub runs no workflows on).
  Either way, run `gh pr checks <n>`.
- **Tests for the new branches**, not only the ones the spec listed. Break
  the main fix in a scratch copy and confirm a test fails.
- **Claims are not evidence.** Code comments ("deliberately untested"),
  commit messages and "addressed in <sha>" replies are claims to verify.
- **Numbered artifacts** (migrations above all) against what has landed on
  the default branch since the task started.
- **New messages and states**: read each one with the values its own path
  passes, and check every new state has a way out.
- **Your repo's own recurring review findings**, if you know them.

**The verdict is a PR comment**, not a GitHub review: `/approve` or
`/request-changes` alone on its own line, with your review above it. You can
also answer with `choco task send <id> --text "..."` carrying exactly one of
the two markers on a line of its own; a reply with neither or both is
refused, and nothing is posted to the PR.

- Only comments newer than the head commit count. Editing an earlier comment
  to add the marker counts too.
- A marker inside a fenced code block still votes. When you quote the
  convention, indent it or break it up.
- Only `OWNER`, `MEMBER` and `COLLABORATOR` accounts vote, and `[bot]`
  accounts never do. Anything commenting under your account, including an
  agent, votes as you.
- Prose doesn't retract a verdict, and the first one the poll sees is acted
  on within a minute. To change your mind, post the other marker inside that
  minute.

**Batch your findings into one `/request-changes`.** Each one costs a coder
lap and a review lap. Prefer approve-and-file-a-follow-up for minor points.

**Your vote isn't charged against earlier internal rejections.**
`internal_review`'s count starts over every time it approves, and a task only
reaches you after an approval. What does accumulate is your own votes: the 4th
since the last escalation parks the task. `choco task status` shows the
current counts on its `Loop counters` line.

**The coder often ignores your `/request-changes` comment** and works from
the internal reviewer's earlier summary instead. After each lap, check that
each of your items maps to a commit. Watch for an **empty commit**: the coder
makes one when it decides only the PR description needs changing, and it
moves the head past your comment without changing anything. What to do next
is in [reference/recover.md](reference/recover.md).

**`/approve` moves the task to `done`; merging the PR is still your job.**
Before you merge, check what it will close:

1. `gh pr view <n> --json closingIssuesReferences` shows what the PR body
   closes.
2. Commit messages are not covered by that, and each one lands with a merge
   commit and closes what it names. Check the pushed branch (the PR's
   `headRefName`); the task's worktree is gone once it is `done`. No output
   means no commit closes anything:

   ```bash
   git fetch origin
   git log --format=%B origin/main..origin/<head-branch> \
     | grep -inE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[*_]*:?[*_]* +(([a-z0-9_.-]+/[a-z0-9_.-]+)?#[0-9]+|https?://github\.com/[^ ]+/issues/[0-9]+)'
   ```

## 5. Recover

Read [reference/recover.md](reference/recover.md) when a task is `stuck`,
parked at `escalate_to_human`, ignoring your review items, or needs
cancelling. In short:

- `stuck` → find out why in `choco task status <id>`, then
  `choco task retry <id>`.
- `escalate_to_human` → `choco task send <id> --text "<note>"` is the only
  way on; `/approve` does nothing there.
- `choco task cancel <id>` is final and deletes the task's worktree and
  local branch; `--keep` keeps both for you to take over.

## Cost and safety

- **Every stage runs the real `claude` CLI.** Don't create a task to try
  something out. For a first run, pick a small, real change.
- **A repo's `.chocofactory/workflows/` and any `--workflow` file can run
  shell commands as you.** Pointing choco at a repo trusts its workflows.
- **Cancel a task that is going round in circles** rather than letting it
  spend a coder lap per round.
- **`choco update`, `choco server stop` and `choco server restart` refuse
  with exit 3 while an agent turn or shell step is running.** `--force` goes
  ahead and marks that work `stuck`; use it only if you are prepared to
  `choco task retry` those tasks.
