# Writing and customising workflows

A workflow is a YAML file that defines a task's stages, roles and routing. This page covers where a workflow comes from, how to keep your own in a repo, and how to write one: routing on an agent's verdict, what an agent can see, read-only roles and human gates. Back to the [README](../README.md).

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
one outcome it advances on.

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
  marked `stuck`.
- Once a turn has reported and ended, its process has 30 seconds to exit. If
  it's still running, its whole process group is killed and the task is
  marked `stuck`, rather than advancing past work that may still be landing.
- Output that arrives after a turn completed stays on the timeline, flagged
  `after_completion`. Nudges and kills appear as `session_note` events.

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
  single-shot `agent_turn` with an `on:` map). A `read_only` role on a standing
  stage (`on: {}`, as in a chat) gets a baseline but its turns are not checked.

Two rules are checked at load time: a `read_only` role must list all three
names in `disallowed_tools`, and `read_only` needs `worktree: true` on the
workflow, so the check only ever looks at the task's own disposable worktree.

Neither field can be set from task config (`--config`, `--role-*`) or the
global config file; such keys are ignored. Only the workflow definition can
set them.

If the turn changed the worktree anyway, the task is marked `stuck` with a
reason such as `read-only role 'reviewer' changed the worktree in stage
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
  `script_file`, `env`, `interval`, `timeout`, `outcomes`), and `interval` is
  required. When an outcome matches, the gate advances on it, keeping the
  command's output if the gate says `capture: text`. When `timeout` runs out it
  advances on the `timeout` edge, which `on:` must have. The watcher is the
  same loop a `poll` runs: it survives a daemon restart with its stored
  deadline.
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
- A gate without `markers:` takes any reply and resumes on `resumed`.
- An accepted reply stops the watcher.
