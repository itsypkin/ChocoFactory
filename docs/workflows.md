# Writing and customising workflows

A workflow is a YAML file that defines a task's stages, roles and routing. This page covers where a workflow comes from, how to keep your own in a repo, and how to write one: routing on an agent's verdict, what an agent can see, read-only roles, human gates and parallel stages. Back to the [README](../README.md).

## Project workflows

A project can carry a repo of its own (`repo_path`), set at creation or
after the fact:

```
choco project create acme --repo ~/code/acme
choco project update acme --repo ~/code/acme   # or: --no-repo, to clear it
```

`--repo` is resolved to an absolute path client-side (a relative path,
including `.`, works) before the request is sent, and must already be an
existing directory — it does not need to be a git repo.

A task's workflow comes from **one of three places, checked in this
order**:

1. an explicit path: `choco task create --workflow <path-to.yaml>` (anything
   containing `/` or ending in `.yaml`/`.yml`; its prompts and scripts
   resolve next to it);
2. the project's repo: `<repo_path>/.chocofactory/workflows/<name>.yaml`, if
   the project has a repo;
3. the built-in of that name, embedded in the daemon binary.

The first match wins. A repo's `.chocofactory/workflows/` has one
`<name>.yaml` per workflow, plus a
`prompts/` directory and a `scripts/` directory next to it for any prompt or
script files the workflow references by relative path (`prompts/coder.md` in a workflow at
`<repo>/.chocofactory/workflows/coding-task.yaml` resolves to
`<repo>/.chocofactory/workflows/prompts/coder.md`). This is the whole point of a repo workflow: a team's own
stages, roles, models and prompts, reviewed and versioned alongside the
code they work on, rather than living only on whoever's machine runs the
daemon.

A task created with no `--repo` of its own defaults to the project's
`repo_path` (as `config.cwd`), so registering a repo on a project is also
what makes `--repo` optional on every task under it. Without either, a
worktree workflow (all the coding built-ins) can't start.

`choco project init-workflows <project>` seeds a repo with all three built-in
workflows (`chat`, `coding-task` and `coding-task-planned`) and their
`prompts/` and `scripts/` as a starting point —
never overwriting a file already there, so it is always safe to run again:

```
$ choco project init-workflows acme
Seeded /Users/you/code/acme/.chocofactory/workflows
  created   /Users/you/code/acme/.chocofactory/workflows/chat.yaml
  created   /Users/you/code/acme/.chocofactory/workflows/coding-task.yaml
  created   /Users/you/code/acme/.chocofactory/workflows/coding-task-planned.yaml
  created   /Users/you/code/acme/.chocofactory/workflows/prompts/coder-system.md
  created   /Users/you/code/acme/.chocofactory/workflows/prompts/coder-turn.md
  ...

Commit this directory so the team shares it: git add .chocofactory/ && git commit
```

From there, edit the seeded files freely (rename `coding-task.yaml` to
something like `express-sonnet.yaml`, add a second `deep-opus.yaml` with
different roles/models — a workflow file *is* the unit of configuration
here, there is no separate project-settings layer) and commit
`.chocofactory/` so every teammate's `choco task create --workflow ...`
resolves the same file.

**Trust implication:** a repo's `.chocofactory/workflows/` can define
`shell` stages, which the daemon runs as ordinary subprocesses on whatever
machine it's on. Registering a repo on a project means trusting everything
under that repo's `.chocofactory/` the same way you'd trust a Makefile or
CI config in it — there is no separate approval step before those commands
run.

## Which workflow file a task ran

Every task records the exact workflow file it started from — its
canonical absolute path and a SHA-256 of its contents at that moment — and
`choco task status` shows it:

```
$ choco task status bb93ada3-...
...
Workflow       coding-task
Workflow file  /Users/you/code/acme/.chocofactory/workflows/coding-task.yaml  [3f2a9c1e0b7d]
Status         open
```

A task running a built-in records `builtin:<name>@<version>` instead of a
path, and `choco task status` labels that line `Workflow`. A built-in task
follows the daemon: it runs whatever the current binary ships, and status
says "built-in updated since task start" when that differs from what it
began with.

If the file has since been edited, the line is suffixed
`(changed since task start)`; if it has been deleted, `(missing)` — either
way the task keeps running (or, for a deleted file, keeps failing to
reload) with no separate warning elsewhere. A task created before this
existed shows no `Workflow file` line at all.

## Customising workflows

- **Eject.** `choco project init-workflows <project>` copies the built-ins in
  this version of the daemon into the project's repo as a starting point.
  Later upgrades don't change the copies.
- **A file of your own.** `choco task create --workflow <path-to.yaml>` runs
  that file as is, without putting it in a repo. To try an unmerged change to
  a built-in, point it at the checkout:
  `--workflow <checkout>/workflows/coding-task.yaml`.

## Migrating from the old global folder

Older versions copied the
built-ins into `~/.config/chocofactory/workflows/` and never updated
them. That folder is no longer read. At startup the daemon logs how many
stale built-in copies it is ignoring, and warns about every other file in
it (an edited copy, a custom workflow) with the two ways to keep using it:
pass the file with `--workflow <path>`, or move it with its `prompts/` and
`scripts/` into a repo's `.chocofactory/workflows/`. It also warns when
open or stuck tasks still point into the folder. The daemon never
modifies or deletes anything there; delete it yourself once no task uses
it. Tasks that already recorded a path into it keep running that file.

## Security

A repo's `.chocofactory/workflows/` and any `--workflow` file
can run shell commands as you. Pointing choco at an untrusted repo or file
trusts its workflows.

## Routing on an agent's verdict

Every agent turn is launched with an MCP tool, `report_outcome`, that lets
the agent state its verdict explicitly instead of the engine trying to guess
one from its reply's text. The whole rule a workflow author needs is one
sentence:

> A stage routes on the agent's own verdict **if and only if** it declares
> `capture: json`. Its `on:` keys are the allowed verdicts.

That's it — nothing about the tool belongs in a prompt file. Given

```yaml
internal_review:
  kind: agent_turn
  role: reviewer
  capture: json
  on: { approved: open_pr, changes_requested: revising }
```

the daemon derives, from `on:`'s keys alone: the tool's allowed `outcome`
values, its description, and (via `--append-system-prompt`) the instruction
telling the agent to call it before ending its turn. There is no second copy
of `approved`/`changes_requested` to keep in sync — change the `on:` map and
every agent-facing part of the contract changes with it.

The tool is present on *every* agent turn, and every stage that can finish
on its own (anything but a standing `on: {}` session like chat) has to call
it to finish. A stage without `capture: json` may only report `done`, the
one outcome it advances on. A branch of a [parallel group](#parallel-stages)
reports one of its `results:` instead of an `on:` key.

A stage can also require the report itself to carry named sections:

```yaml
internal_review:
  kind: agent_turn
  role: reviewer
  capture: json
  report_sections: [Branches → tests, States, Findings, Dismissed]
  on: { approved: open_pr, changes_requested: revising }
```

Each name must appear as a heading in the report's `summary`, with
something under it (`Findings: none` counts). Headings are matched
forgivingly — `## Findings`, `**Findings**`, `- Findings`, `1. Findings`
and `Findings:` are the same thing, `->` and `→` are interchangeable, and
a bullet that merely *starts* with a section's name ("- Side effects of
the retry are untested") is a list item, not a heading. A report that
leaves a section out is rejected with an error the agent can act on and
call again.

The list reaches the agent through the tool's own schema, so a stage that
opts in needs no prompt changes to work. A prompt that explains the
sections anyway — `coding-task`'s reviewer does — is a second copy of the
list, and a test keeps the two from drifting.

This is how a verdict is kept from being cheaper than the work behind it: a
reviewer that stops at its first blocking finding has no walk to write down. Because parking a turn costs a human, the rule bends
before it breaks — after two rejections the report is recorded as it stands, with the
missing sections named in the tool's reply on the task's timeline.

### When a turn counts as complete

The CLI's end-of-turn line doesn't mean the work is done: an
agent waiting on a background sub-agent or a long test run ends its turn and
is woken when that finishes. So the daemon treats a turn as complete only
when it ends *after* the agent called `report_outcome`:

- A turn that ends without reporting is left open. After 5 minutes with no
  output it's nudged (up to 3 times); after that it's closed, and the task is
  marked `stuck`. (In a [parallel group](#parallel-stages), where this section
  says the task is marked `stuck`, the branch fails instead, and the task
  parks once no branch is running.)
- The exception is a turn whose own background job is still running. On
  Claude Code, the CLI reports the session's running background jobs, and
  while any are running a report-less turn is not nudged. A `job_wait`
  `session_note` marks the start of each wait. The 60 minutes are a total for
  the whole turn, summed over all its waits: a wait that begins after the CLI
  woke the agent and it ended again without reporting gets only what is left,
  never a fresh 60 minutes. When the total is used up the turn is closed as
  `no_report` with a `session_note` that names the jobs; once the jobs are
  gone the nudge rule above applies again. Roles on omp keep the nudge rule.
- Once a turn has reported and ended, its process has 30 seconds to exit. If
  it's still running, the process and what the turn started are killed and the
  task is marked `stuck`, rather than advancing past work that may still be
  landing. A process that exited on its own, with nothing of the turn's left
  alive, does not park the task.
- Output that arrives after a turn completed stays on the timeline, flagged
  `after_completion`. Nudges and kills appear as `session_note` events.
- However a turn ends (reported, closed, cancelled, daemon stopped, or the
  agent crashed), choco then kills the processes it can prove the turn
  started. That includes background jobs the agent's tool ran in sessions
  and process groups of their own (Claude Code runs every Bash call in a
  new session), such as a hung `cargo test` or a dev server. They are
  listed in a `leftovers_killed` `session_note`, which also lists any that
  could not be killed. A `leftovers_unchecked` note means choco could not
  read the process table, so the check could not run (the agent's own
  process group was still killed). A process counts as the turn's if it
  descends from the agent, was seen descending from it earlier, carries
  the turn's marker in its environment, or shares a session with one that
  does. choco never signals anything else. One blind spot: on macOS the OS
  hides the environment of Apple's own binaries (`/bin/zsh`, `/bin/sleep`,
  `/usr/bin/perl`), so a foreground `nohup /bin/sleep 999 &` that left the
  agent's process tree before the turn ended, and shares no session with
  anything else of the turn's, survives. Anything you built or installed
  (cargo, test binaries, node, Homebrew Python) is found. A process started
  through a service manager (`launchctl`, `systemd-run`, `docker run`) or
  as another user is out of reach. Two more blind spots: a process that
  drops its environment (`env -i`) and leaves both the agent's process tree
  and its session before choco has seen it descend from the agent is not
  found; and on Linux, a process whose `/proc/<pid>/environ` cannot be read
  (setuid/setgid programs, or one that made itself non-dumpable) carries no
  readable marker, so it is found only through the tree, an earlier sighting
  or a shared session.

A sub-agent calling `report_outcome` doesn't count, and neither does a call
the tool rejected.

## What an agent inherits from your Claude setup

By default an agent role runs isolated from the operator's own Claude Code
setup: no `~/.claude/CLAUDE.md`, no user plugins, hooks or output style, no
MCP servers other than the daemon's, no skills, no auto-memory, no
built-in `ReportFindings` tool, and no timer or wait tools (`ScheduleWakeup`,
`Monitor`, `CronCreate`, `CronDelete`, `CronList`, `RemoteTrigger`):
an agent that ends its turn to wait on its own timer races the daemon's
nudge clock. Chat keeps `ScheduleWakeup` and `Monitor` but can't use the cron
or remote-trigger tools. The task repo's own `CLAUDE.md` and `AGENTS.md`
files are read: the root ones at start, nested ones when the agent reads a
file in that folder. The scope is the role's working directory, not the git
root, so a custom `worktree: false` workflow whose task runs in a repo
subfolder doesn't get the repo-root files (the built-ins all use worktrees).
Instruction files in folders above the repo are not,
and neither is `~/.claude/CLAUDE.md`. `AGENTS.md` depends on Claude
Code's built-in agents-md plugin; if a session's plugins don't include it, an
`error` event on the timeline (and a daemon-log warning) says so. The repo's
`.claude/settings.json` still applies, including any hooks or plugins that
repo enables: they belong to the code being worked on. The repo's
`.claude/settings.local.json` is *not* read: it is the operator's personal
file, and in a task's linked worktree Claude Code would resolve it to the
main checkout's. A role can loosen the
rest in the workflow file:

```yaml
roles:
  coder:
    skills: [run-tests]   # skills it may invoke; omitted = none
    memory: true          # use auto-memory; omitted = no
  chat:
    inherit_operator_config: true   # your full setup, minus cron/remote-trigger tools
```

`skills`/`memory` can't be combined with `inherit_operator_config`. None of
these can be set from task config (`--config`, `--role-*`) or the global
config file: only a workflow definition can loosen what its agents see. The
built-in `chat` workflow inherits your setup; `coding-task` is isolated. A
copy of `chat.yaml` you made yourself (in a repo, or run with
`--workflow <path>`) needs `inherit_operator_config: true` added by hand to
keep that behaviour.

Each session's `session_meta` event records what it actually ran with: the
CLI version, model, tools, MCP servers, plugins, skills, and the isolation
it was launched under.

## Read-only roles

A role that must not change the code (a reviewer, a planner) can be enforced
rather than just asked nicely. Two role fields do it:

```yaml
worktree: true
roles:
  reviewer:
    cli: claude
    model: claude-opus-5-5
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
```

- `disallowed_tools` removes tools from the role. The names are
  adapter-neutral: `edit`, `write` and `notebook_edit`, exact lowercase only;
  anything else is rejected when the workflow loads. The `claude` adapter maps
  them to `Edit`, `Write` and `NotebookEdit`. Duplicates are dropped.
- `read_only: true` makes the daemon snapshot the task's worktree (HEAD, the
  branch, `git status` and file contents, ignored files excluded) before the role's turn, and
  compare it afterwards. Bash can still write files, so the denylist alone
  isn't enough. The comparison happens only on stages that conclude (a
  single-shot `agent_turn` with an `on:` map), and on every branch of a
  [parallel group](#parallel-stages) (whose `on:` is always empty). A `read_only` role on a standing
  stage (`on: {}`, as in a chat) gets a baseline but its turns are not checked.

Two rules are checked at load time: a `read_only` role must list all three
names in `disallowed_tools`, and `read_only` needs `worktree: true` on the
workflow, so the check only ever looks at the task's own disposable worktree.

Neither field can be set from task config (`--config`, `--role-*`) or the
global config file; such keys are ignored. Only the workflow definition can
set them.

If the turn changed the worktree anyway, the task is marked `stuck` (in a
[parallel group](#parallel-stages) the branch fails instead, and the task parks
when the group settles) with a reason such as `read-only role 'reviewer' changed the worktree in stage
'internal_review': HEAD 1a2b3c4 → 9f8e7d6; git status changed (3 entries)`,
and a `worktree_changed` event lands on the timeline. Nothing is reverted:
inspect the worktree, reset it yourself, then run `choco task retry`. A
resumed turn is compared against the baseline of the session it resumes. If
the check can't run (git fails), the task is parked too, never passed
silently. The comparison also runs when the turn crashes, ends without
reporting, or is cut off; the stuck reason then carries both facts. Only a
resumed session keeps its baseline: any other retry starts a fresh session
that baselines whatever is in the worktree, so reset it before retrying.
The check covers HEAD, the branch, `git status` and file contents. It
doesn't cover ignored paths (`target/`, `.omc/`) or anything inside `.git`
(refs, config, hooks). A read-only turn that runs `cargo fmt` or rewrites `Cargo.lock`
trips the check as well, and that is intended. The built-in reviewer, and the
planner in `coding-task-planned`, are read-only.

## A human gate that watches for its answer

A `human_gate` normally waits for a reply through `choco task send`. It can
also watch for its answer somewhere else, and it can require that a reply
carries a verdict.

```yaml
awaiting_review:
  kind: human_gate
  capture: text
  watch:
    command: "gh api …"
    interval: 30s
    timeout: 24h
    outcomes:
      - match: "APPROVE"
        then: approved
  markers:
    - line: /request-changes
      then: changes_requested
    - line: /approve
      then: approved
  on: { approved: done, changes_requested: coding, timeout: stalled }
```

- `watch:` takes the same fields as a `poll` stage (`command` or
  `script_file`, `env`, `interval`, `backoff`, `timeout`, `outcomes`), and
  `interval` is required. When an outcome matches, the gate advances on it, keeping the
  command's output if the gate says `capture: text`. When `timeout` runs out it
  advances on the `timeout` edge, which `on:` must have. The watcher is the
  same loop a `poll` runs: it survives a daemon restart with its stored
  deadline.
- `backoff:` (optional, on a `poll` stage and on `watch:`) slows the polling
  down the longer the stage waits. Each step is `{ after: 6h, interval: 5m }`:
  from `after` on, the step's `interval` replaces the base one. `after` is
  measured from when the stage was entered, so it survives a daemon restart.
  Durations are `s`, `m` or `h` only (write `72h`, not `3d`). Steps must have
  strictly increasing `after` values, each before `timeout` when there is one,
  and the list can't be empty. `backoff` never moves the deadline: `timeout`
  stays one budget counted from stage entry.
- `markers:` makes a reply through choco carry a verdict. Each entry is a
  `line` and the outcome (`then`) it chooses. A reply counts a line as a marker
  when the whole line equals it: case-sensitive, trailing spaces, tabs and
  carriage returns ignored, leading whitespace not. So `> /approve`,
  `use /approve here` and `  /approve` are not markers.
- A reply is refused, with nothing recorded and the watcher still running, when
  it has no marker line, or when its markers choose different outcomes. The
  same marker twice is fine.
- On an accepted reply the gate advances on the marker's outcome. The captured
  text is the reply without its marker lines. The timeline's `human_message`
  event keeps the reply as typed, and names the outcome.
- A gate without `markers:` takes any reply and resumes on `resumed`, except
  a reply made up only of another gate's marker lines (say `/approve` alone):
  that is refused, with nothing recorded, because it would be taken as a note
  rather than a verdict.
- An accepted reply stops the watcher.

## Parallel stages

A `kind: parallel` stage starts several agent turns (its *branches*) at the
same time, waits for all of them, and then moves on. Use it when independent
reviewers can read the same commit side by side. This example is
self-contained; its prompt files live next to the YAML, under `prompts/`:

```yaml
name: panel-example
worktree: true
roles:
  coder:
    cli: claude
    model: claude-sonnet-5-5
  security:
    cli: claude
    model: claude-sonnet-5-5
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
  architect:
    cli: claude
    model: claude-sonnet-5-5
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
  lead:
    cli: claude
    model: claude-opus-5-5
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]

stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: prompts/coder.md
    on: { done: review_panel }

  review_panel:
    kind: parallel
    branches:
      security_review:
        kind: agent_turn
        role: security
        prompt_file: prompts/security.md
        capture: json
        results: [clean, blocking]
      architecture_review:
        kind: agent_turn
        role: architect
        prompt_file: prompts/architecture.md
        capture: json
        results: [clean, blocking]
    on: { done: lead_review }

  lead_review:
    kind: agent_turn
    role: lead
    prompt_file: prompts/lead.md
    capture: json
    on: { approved: done, changes_requested: coding }

  done:
    kind: terminal
```

`prompts/lead.md` would contain `{{ stages.security_review.summary }}` and
`{{ stages.architecture_review.summary }}` (and the `.outcome` of each) so
the lead sees both reports.

The loader enforces these rules, and each failure names the stage:

- `branches:` is a map of at least two: `parallel stage '{stage}' has {count}
  branch(es), but a group needs at least two`.
- The group has exactly `on: { done: <stage> }`: `parallel stage '{stage}'
  must have exactly one 'on:' key, 'done'`. It has no `loop_guard`: `…has a
  'loop_guard', which a group does not support`.
- A branch is an `agent_turn`. Shell and poll branches are not supported yet:
  `branch '{branch}' of parallel stage '{group}' is a {kind} stage; {kind}
  branches are supported in a later version, only agent_turn branches are for
  now`. A `parallel`, `human_gate` or `terminal` branch is never allowed:
  `…is a {kind} stage; a branch must be an agent_turn`. So groups don't nest.
- A branch has no `on:` and no `loop_guard`. A group's own `done` edge is the
  only way out.
- Stage and branch names are unique across the whole workflow.
- No `on:` target and no `loop_guard.then` may name a branch: `…routes to
  '{target}', which is a branch of parallel stage '{group}'; route to the
  group instead`.
- Every branch role is `read_only` (see below).
- `prompt_file`, `capture` and `report_sections` work on a branch as they do
  on a stage.

### `results:`

`results:` lists the outcomes a branch may report. None of them route. The
default is `[done]`. Anything other than `[done]` needs `capture: json`, the
list can't be empty and can't repeat a value. The `report_outcome` tool's
allowed values come from it, as they come from `on:` on a stage; a branch
without `capture: json` may only report `done`.

### The join

Every branch starts at once. The group waits for every branch, then always
leaves through `done`, whatever results the branches reported. Any decision
belongs to a later stage that reads the branches' captures. Entering the group
again (after a revise lap, say) runs every branch again.

### When a branch fails

A branch fails when its turn can't complete: it ends without reporting (reason
such as `stage 'security_review': the agent's turn ended without calling
report_outcome`), crashes, leaves a process running, changes the worktree, or
hits a usage limit. It also fails if it reports a result outside its
`results:`, and its capture is then dropped (the tool normally refuses such a
value, so this is a guard). The other branches keep running. When none is left
running, the task is marked `stuck` with a reason naming each failed branch,
joined with `; `:

```
parallel stage 'review_panel': 1 of 3 branch(es) failed: 'security_review': stage 'security_review': result 'maybe' is not one of its results [clean, blocking]
```

`choco task cancel` kills every branch's session.

### Retrying a group

`choco task retry <id>` re-runs only the failed branches. Done branches keep
their results and captures and are not paid for again. Each failed branch
resumes its interrupted session or starts fresh, by the same rules as a stage;
a branch whose last session belongs to an earlier entry of the group starts
fresh.

- `--resume` is refused, changing nothing, if any failed branch can't resume:
  `branch '<b>' of parallel stage '<g>': <why>`.
- `--fresh` starts every failed branch fresh.
- A retry is refused while any branch's session is still live: `branch '<b>'
  of parallel stage '<g>' still has a live session; wait for it to end, then
  retry (or cancel the task)`.
- If no branch failed it is refused: `every branch of parallel stage '<g>' is
  done; there is no failed branch to retry`.

```
Retrying parallel stage 'review_panel': re-running 2 failed branch(es); finished branches are kept.
  security_review: resuming its interrupted session (sess-1).
  architecture_review: from scratch, in a fresh session: <why>.
See `choco task status <id>`.
```

A branch line may also end `resuming its interrupted session.` or `from
scratch, in a fresh session.`, without the parenthesis or the reason.

### Watching a group

`choco task status` shows a `Branches` table while the task is in the group;
see the **Parallel groups** paragraph in
[cli.md](cli.md#a-full-walkthrough) for its columns. The dashboard's stage
cell reads `<group> settled/total`. `choco task events` has one line per
branch start and finish:

```
review_panel › security_review  started (agent_turn, entry 1)
review_panel › security_review  done: clean (entry 1)
review_panel › architecture_review  failed: <reason> (entry 1)
```

A start after a retry ends `, via retry` or `, via retry_resume`.

### When the daemon restarts

If the daemon restarts while a group runs, each running agent branch's session
is marked `daemon_stopped` and the branch becomes `failed` and resumable. With
nothing left running the group settles, so the task parks `stuck`, and `choco
task retry` then resumes exactly those branches. (This describes the approved
design; it is being implemented separately and may not be in your build yet.)

### Branches must be read-only

Branches share one worktree and run concurrently, so each branch's role must
be `read_only: true`, or loading fails: `branch '{branch}' of parallel stage
'{group}' uses role '{role}', which is not 'read_only: true'; branches run
side by side so they must not edit the worktree`. Each branch's turn is
checked against its own baseline, as any [read-only](#read-only-roles) turn
is. A violation fails that branch rather than parking the task at once; its
reason is the usual `read-only role '…' changed the worktree in stage
'<branch>': …` and ends `; ran beside: <other branches>`. Build output in
ignored folders, and anything inside `.git`, is outside the check.

### Templates

A later stage reads a branch with `{{ stages.<branch>.<field> }}`. A branch
may read its own previous capture (as a re-reviewer does) and anything
captured before the group was entered. It may not reference a sibling:
`branch '{branch}' of parallel stage '{group}' has {placeholder} in its
{field}, but '{sibling}' is a sibling branch that runs at the same time, so
its result does not exist yet` (the only value it could see would be last
lap's). `{{ stages.<group>… }}` is an error because a group captures nothing:
`stage '{stage}' has {placeholder} in its {field}, but stage '{referenced}'
declares no 'capture:' so it stores nothing to reference`. `{{
left_at.<branch> }}` is not supported (`…but '{referenced}' is not a stage in
this workflow`); `left_at.<group>` works.

### What it costs

There is no cap on branches. N branches are N agent processes at once on one
vendor account, so its rate limit is reached N times as fast (a usage-limit
cut-off parks the branch as resumable). Whatever a branch's prompt does
happens N times concurrently: if the prompts build the project, that is N
builds. Bound how many branches build; the example below lets exactly one.
Concurrent branches can't share one scratch build directory, because they
would race to create it. Re-entering the group re-runs every branch, so each
lap of a review loop costs every branch again.

### An example

`workflows/experimental/review-panel.yaml` is a full coding workflow with a
three-branch review panel and a lead who decides. It is experimental and not
built in. Run it with `choco task create --workflow
<checkout>/workflows/experimental/review-panel.yaml …`.
