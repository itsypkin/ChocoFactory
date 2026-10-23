---
name: run-choco-task
description: Drives a coding task through a ChocoFactory (choco) daemon end to end — check choco and its daemon (updating it when it is yours), write the task spec, create and watch the task, review its PR and cast the verdict, and recover a stuck, escalated or interrupted task. Use when asked to have choco implement an issue, or to run, watch, review or rescue a choco task.
---

# Run a choco task

You are the operator. Choco's agents write and review the code; you decide
what they are asked for, whether the result is good, and what happens when
they get stuck. Every stage runs the real `claude` CLI, so every lap costs
real money: read [Cost and safety](#cost-and-safety) before you start.

Flags are in `choco <command> --help`; this skill is the judgement around
them.

The built-in `coding-task-planned` workflow is the default for coding work;
plain `coding-task` is for when you have already checked the spec against the
code yourself:

```
[spec_check ⇄ spec_questions →] coding → internal_review → open_pr → checks_polling → awaiting_human_review → done
```

The bracketed part is `coding-task-planned` only.

Every way back goes through `revising`, which returns to `internal_review`:
`internal_review` requests changes, `checks_polling` sees a failed check, you
post `/request-changes`, or someone resumes the task from `escalate_to_human`.

A task parks at `escalate_to_human` after a 4th rejection in a row by
`internal_review`, a 4th red CI result in a row from `checks_polling`, a 4th
`/request-changes` from you since the last escalation (each counted
separately), after about 4 days with no verdict from you, when CI has not
finished in 30 minutes (after a last look, and up to 30 more minutes if the
machine slept through the deadline), when a check is cancelled, failed to start or needs
an action (approval) before it can run, or when `open_pr` fails.

Copy this checklist and tick it off:

```
- [ ] 1. Prepare: choco up to date, or a shared daemon's version noted; daemon running
- [ ] 2. Write the spec
- [ ] 3. Create the task and wait for awaiting_human_review (or a parked state)
- [ ] 4. Review the PR, vote, run the pre-merge checks, and wrap up after the merge
- [ ] 5. Recover if it got stuck or escalated
```

## 1. Prepare

**Update choco first, if the daemon is yours.** This skill describes the
latest release. Agents' prompts ship inside the daemon, so an older daemon
behaves differently from what follows.

```bash
choco update             # installs the latest release and restarts a daemon running from the install folder
choco --version
```

- **A daemon shared with other operators, or pinned to a version by someone
  else:** don't update, stop or restart it; that would kill their work. Run
  `choco server status`, note the daemon's version from its first line
  (`chocofactoryd <version> … running`), and go on. Behaviour can then
  differ from what this skill describes.
- `choco: command not found`: install it as in the
  [README](https://github.com/itsypkin/ChocoFactory#install), then go on.
- `choco update` refuses while work is in flight, meaning an agent turn or
  shell step that is running. A turn only waiting for an agent slot is not in
  flight; see [Cost and safety](#cost-and-safety).
- It refuses with exit 1 for a copy that wasn't installed by the install
  script; the error says what to run instead.

**First run.** Once per machine and repo, from the repo choco will work on:

```bash
claude --version              # agents run the claude CLI, logged in as you
gh auth status                # open_pr and the polls run gh as this account; it is who "you" are when voting
choco server start && choco server status
choco project list            # or: choco project create <name> --repo .
```

**Before each task**, check the daemon:

```bash
choco server status
```

Replace `origin/main` with your default branch if it differs, in the create
command in step 3 and the pre-merge check in step 4.

- **A task forks from `--base <ref>`.** Without it, from the remote's default
  branch, freshly fetched (`origin`, or the only remote), or from the repo's
  HEAD when the repo has no remote. A `<remote>/<branch>` base is fetched
  first; any other ref is used as the repo has it. A ref that doesn't
  resolve, or a failed fetch, creates nothing. `choco task status` shows the
  `Base` line.
- **Agent caps.** A cap on how many agent turns run at once is off by
  default. Set the server's with `max_agents: N` in
  `~/.config/chocofactory/config.yaml`, and one project's own with
  `choco project create|update --max-agents N` (`0` removes it; a project with
  a cap is its own pool and doesn't use the server's slots). Unset or `0`
  means no limit, and an edit applies within seconds, without a restart. After
  editing, read the `agent slots` line of `choco server status`: it always
  shows the server pool's limit (`no limit` when unset), so a mistyped key
  shows up there. A turn waiting for a slot reads `Queued` in `choco task
  status` and `· queued N` in `choco task list`; it is waiting, not stuck
  (see [reference/watch.md](reference/watch.md#a-turn-waiting-for-an-agent-slot)).
- **`choco server status`** shows the daemon's version, open tasks,
  in-flight work and agent slots (each pool's running count, limit and
  queued turns). It warns when `choco` and the daemon differ in version, or
  the daemon binary changed on disk since it started: run
  `choco server restart` then if the daemon is yours, otherwise tell its
  owner.
- **`choco task status <id>`** shows which workflow a task runs
  (`builtin:<name>@<version>`, or a file path with a hash) and marks it
  `(built-in updated since task start)`, or `(changed since task start)` for
  a workflow file, when it changed. Check that line and the
  daemon's version before blaming a prompt for a run's behaviour. A workflow
  file's name line reads like a built-in; see
  [reference/watch.md](reference/watch.md#the-workflow-file-line).
  A task in a parallel group shows a `Branches` table under its `Stage`
  line: branch, kind, state, result or reason, time and cost. `no data` in
  cost means no turn recorded usage for that branch's entry, and `-` in time
  means the branch is not running (the task is not open) or a time is
  missing. `choco --json task status <id> | jq '.workflow_state.branches'`
  gives the same as JSON. The dashboard's stage cell reads
  `<group> settled/total`, and `choco task events` has one line per branch
  start and finish. See
  [reference/watch.md](reference/watch.md#reading-a-parallel-group).
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
  that name, and choco updates don't change it (`choco project
  init-workflows` creates exactly that folder). To change a workflow, see
  the [customize-choco-workflow skill](../customize-choco-workflow/SKILL.md).
- **Don't stop or restart the daemon while an agent turn (including a
  parallel group's running branches) or shell step runs.** `stop` and
  `restart` refuse (see [Cost and safety](#cost-and-safety)).
  Tasks waiting on a poll or a human survive a restart, and so do turns
  queued for an agent slot: they don't block `update`, `stop` or `restart`,
  and they start in their order after the restart.
  A forced restart while a parallel group's branches run parks the task
  `stuck` with those branches failed; `choco task retry` resumes them
  where it can and starts the rest fresh.
  The daemon logs to `~/.config/chocofactory/logs/chocofactoryd.log`; kills,
  `stuck` marks and nudges show up there first.
- **Keep the machine awake for the whole run.** On a sleeping laptop agent
  turns stall, and the review window (about 4 days) keeps running and can park the
  task overnight. A stage that wakes after its deadline takes a last look, and
  watches up to 30 more minutes if it slept through the deadline, but that does
  not give the lost time back, and a restart drops it. On macOS, run `caffeinate -dims` in a spare terminal. Don't
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
- **Check that the design holds, not only that it builds.** List every way a
  protected event can end and say whether the protection runs on each, and
  carry in any spec checks your project's instruction files name; see
  [reference/spec.md](reference/spec.md#check-that-the-design-holds).
- **Read the issue with comments:** `gh issue view <n> --json title,body,comments`
  (`gh issue view <n> --comments` can print nothing and exit 0).
- **A task that reads an external API:** name the endpoints and fields, check
  them read-only on a real object, and paste what you saw into the spec. A
  task whose coder runs a real third-party CLI needs a budget and opt-in
  tests. Both are in [reference/spec.md](reference/spec.md).
- **Which `--help`:** the installed `choco --help` matches your daemon. For a
  task that documents or changes the CLI, build the default branch and use
  its `--help` (see [reference/spec.md](reference/spec.md#which---help-to-trust)).
- **Check for collisions against your default branch, open PRs and running tasks:**
  numbered files such as database migrations above all, then file names and
  flags, including a file another PR's tests copy or pin. Two tasks that
  both take the next number conflict, and the second PR's CI may not run.
- **`--closes` decides what the PR closes.** Each `--closes <n>` makes the
  PR say `Closes #n` (repeat the flag for several issues); a `#N` anywhere in
  the title, including a trailing `(#N)`, only gives `Refs #N`. For one part
  of a multi-part issue, don't pass `--closes`: merging would close the whole
  issue. `open_pr` also keeps PR titles to 100 bytes, cutting at a word
  boundary and keeping a trailing `(#N)`, so keep the task title well under
  that.
- **Recommended: have choco check the spec before coding**, with
  `--workflow coding-task-planned`. A planning agent checks the spec against
  the code, fixes stale references, decides the design choices your intent
  implies, and asks you only when it can't go on without guessing. The coder
  and reviewer then work from its report, not your `--prompt`. A claim it
  couldn't run read-only is marked **unverified**: treat it as open until the
  coder reports the probe's result. Read the report, answer questions and
  see the rest in [reference/spec.md](reference/spec.md#having-choco-check-the-spec-first-coding-task-planned).
- **Docs-only task:** every link and anchor resolves (from a copy outside the
  repo, for a skill or doc meant to be copied), every command matches `--help`,
  no fact is lost, and for a skill or doc meant to be copied every snippet
  route is taken on a throwaway daemon (by the operator, not the coder, when it
  needs an agent turn or GitHub); see
  [reference/spec.md](reference/spec.md#docs-only-tasks).
- **Text an agent or a reader acts on** (a prompt, a skill, docs): the spec
  carries behavioural acceptance criteria and names the runs that prove them,
  each with a budget and a way out that tells the internal reviewer to carry a
  "not made: <reason>" item to the human gate; see
  [reference/spec.md](reference/spec.md#text-an-agent-or-a-reader-acts-on).

## 3. Create and watch

```bash
choco task create --project <p> --workflow coding-task-planned \
  --title "<what it does>" --closes <n> --base origin/main \
  --prompt "$(cat spec.md)"
```

- Keep `spec.md` outside the repository, in a temp directory, for example.
  The task reads only the text `--prompt` passes, not files in your checkout.
- Per-task overrides beat editing shared config:
  `--role-model <role>=<model>`, `--role-system-prompt-file <role>=<path>`,
  `--role-cli <role>=<cli>`. A model may end in an effort level, for example
  `--role-model coder=claude-sonnet-5-5:xhigh` (`low`, `medium`, `high`,
  `xhigh`, `max`); set effort only this way. See
  [docs/models.md](../../../docs/models.md#effort-level).
- After creating, compare the `Base` line in `choco task status` with the
  commit you wrote the spec against. If they differ and the commits between
  them touch files the spec names, check the spec still holds.
- **Running a role on omp** (`--role-cli <role>=omp`): see
  [reference/spec.md](reference/spec.md#running-a-role-on-omp).
- **Wait for the task to stop.** One command covers the whole run:
  `choco task status <id> --until attention --timeout <dur>`. It returns
  when the task is open at `spec_questions` (the planner needs your answer;
  reply with `choco task send <id> --text "<answers>"`), at
  `awaiting_human_review` (vote, step 4) or at `escalate_to_human`
  (`choco task retry` or `choco task send`, step 5), or when it is paused,
  stuck, cancelled or closed. Its stderr line and exit code say which. The exit codes are in
  [reference/watch.md](reference/watch.md#waiting-with---until); typical
  stage times for choosing `--timeout` are in
  [reference/watch.md](reference/watch.md#stage-times).
- **Read a stage's verdict text** (the latest lap) with
  `choco --json task status <id> | jq -r '.workflow_state.payload.stages.<stage>.summary'`
  for `spec_check` or `internal_review`. Every lap's text, and watching
  commits while a revise lap is still open, are in
  [reference/watch.md](reference/watch.md#reading-a-stages-verdict-text).
- **Find the task's PR** with `gh pr list --head task/<task-id>` (add
  `--state all` once it is merged or closed). The plain `choco task status`
  doesn't print it, but `choco --json task status <id>` carries it as
  `.workflow_state.payload.stages.open_pr.url` once `open_pr` has run.
  Going the other way, `choco task list --search '#<n>'` finds the task behind
  PR `<n>`.
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
  commit messages and "addressed in <sha>" replies are claims to verify. A coder's
  "not re-run" on a run the spec requires is an unmet requirement, unless it
  reported the run as "not made: <reason>" within the spec's budget and way
  out, and finished. Every
  required run has a budget in the spec.
- **Numbered artifacts** (migrations above all) against what has landed on
  the default branch since the task started.
- **New messages and states**: read each one with the values its own path
  passes, and check every new state has a way out.
- **Your repo's own recurring review findings**, if you know them.
- **A docs PR** has no code tests to break; check it against the
  [docs-only criteria](reference/spec.md#docs-only-tasks) and check that the
  spec's acceptance runs were run and reported.

**The verdict is a PR comment or a GitHub review**: `/approve` or
`/request-changes` alone on its own line, with your review above it. A
collaborator's Approve or Request changes review votes by its state. On your
own PR, GitHub only allows a Comment review, so put the marker on a line of
its body. You can
also answer with `choco task send <id> --text "..."` carrying exactly one of
the two markers on a line of its own; a reply with neither or both is
refused, and nothing is posted to the PR. What counts as a vote (the head
commit fence, edits, ties, who can vote, fenced markers) is in
[reference/review.md](reference/review.md).

**Batch your findings into one `/request-changes`.** Each one costs a coder
lap and a review lap. Prefer approve-and-file-a-follow-up for minor points.

**Your vote isn't charged against earlier internal rejections.**
`internal_review`'s count starts over every time it approves, and a task only
reaches you after an approval. What does accumulate is your own votes: the 4th
since the last escalation parks the task. `choco task status` shows the
current counts on its `Loop counters` line.

**The coder often ignores your `/request-changes` comment** and works from
the internal reviewer's earlier summary instead. After each lap, check that
each of your items maps to a commit (while the task is still open, see
[reference/watch.md](reference/watch.md#watching-commits-in-a-revise-lap)). Watch for an **empty commit**: the coder
makes one when it decides only the PR description needs changing, and it
moves the head past your comment without changing anything. What to do next
is in [reference/recover.md](reference/recover.md).

**`/approve` moves the task to `done`; merging the PR is still your job.**
Merging the PR also moves the task to `done` at the gate's next check
(every minute for its first 6 hours, then every 5 minutes, then every 30
minutes after 30 hours), without a vote, so don't post `/approve` after
merging; a PR closed without merging doesn't move the task. `done` deletes the task's local branch only if it was
pushed or merged; the remote `task/<id>` branch stays unless the repo
deletes merged head branches.

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

**After the merge,** check that `choco task status <id>` shows the task
`closed` once the gate's next check has run, and remove what you made for
it outside the repo: spec drafts, probe files, scratch worktrees and any
throwaway daemon.

**When someone else reviews,** drive the task to `awaiting_human_review`
and run the sanity checks above: the PR merges cleanly, CI passed
(`gh pr checks <n>`), and what the PR and its commits close. Then hand the PR
number to the reviewer and stop; don't vote or merge. Anything commenting
under your account votes as you, so don't paste the markers on their own
line in a comment.

## 5. Recover

Read [reference/recover.md](reference/recover.md) when a task is `stuck`,
parked at `escalate_to_human`, ignoring your review items, or needs
cancelling. In short:

- A task showing `Queued` is waiting for an agent slot, not stuck: don't
  `retry`, `cancel` or `send` to it; see
  [reference/recover.md](reference/recover.md#queued-is-not-stuck).
- `pausing` or `paused` → you (or someone) held the task:
  `choco task resume <id>` continues it. `retry` and a `send` to a chat are
  refused on it, and the message names `resume`; a reply at a gate is accepted
  and recorded, and the stage it leads to starts on resume. `resume` itself
  can refuse: at an agent or group stage with nothing to start or wait for it
  marks the task `stuck`, and `retry` then continues it. See
  [reference/recover.md](reference/recover.md#pausing-a-task).
- `stuck` → find out why in `choco task status <id>`, then
  `choco task retry <id>`. On a task stuck in a parallel group, retry
  re-runs only the failed branches and keeps the finished ones, listing
  each branch and whether it resumed (`--resume` is refused if a failed
  branch can't resume; `--fresh` starts every failed branch fresh); it is
  refused while a branch session is still live (wait for it to end, then
  retry, or cancel).
- `escalate_to_human` → after a review timeout, `choco task retry <id>`
  watches again at no cost; after a CI timeout, re-run CI on GitHub, then
  `choco task retry <id>` (if `choco task events <id> --json` shows a
  `CI_RERUN_FAILED is '...'` note instead, fix that value in the workflow file,
  then retry); after any other escalation (a cancelled or
  failed-to-start check, a loop guard), `choco task send <id> --text "<note>"`
  moves on through a coder lap. `/approve` alone is refused there.
- `choco task cancel <id>` is final and deletes the task's worktree and
  local branch; `--keep` keeps both for you to take over.

## Cost and safety

- **Every role runs at effort `high` by default** on both CLIs, which is
  slower and costs more than `medium`; add `:medium` to a role's model to
  lower it.
- **Every stage runs a real agent CLI** (`claude`, or `omp` for a role with
  `cli: omp`). Don't create a task to try something out, except the budgeted acceptance tasks a spec assigns to the operator or another named person (see [reference/spec.md](reference/spec.md#docs-only-tasks)). For a first run, pick a small, real change.
- **A repo's `.chocofactory/workflows/` and any `--workflow` file can run
  shell commands as you.** Pointing choco at a repo trusts its workflows.
- **To free the machine for a while, or to hold one task while another
  finishes, pause it** instead of cancelling it: `choco task pause <id>` lets
  a running turn finish and starts nothing new; `--now` also stops the turn
  (`choco task resume <id>` continues the same session where the CLI can).
- **Cancel a task that is going round in circles** rather than letting it
  spend a coder lap per round.
- **`choco update`, `choco server stop` and `choco server restart` refuse
  with exit 3 while an agent turn (including a parallel group's running
  branches) or shell step is running, and while a `pausing` task still has
  one running (a `paused` task never blocks them).** `--force` goes
  ahead and marks that work `stuck`; use it only if you are prepared to
  `choco task retry` those tasks. Turns queued for an agent slot are not
  running: they never block these commands, and the commands print `N queued
  turns will wait and start in order after the restart.`
