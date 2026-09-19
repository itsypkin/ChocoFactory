# ChocoFactory

Two binaries:

- **`chocofactoryd`** — the daemon. Owns the SQLite database, the workflow
  engine, and an HTTP/WS API on `127.0.0.1:4141`.
- **`choco`** — a thin CLI client against that API (create/inspect/message
  tasks and projects).

## Build

```
cargo build --workspace
```

Binaries land in `target/debug/`.

## Running the daemon

> **`chocofactoryd` spawns the real `claude` CLI by default** — running the
> daemon will hit the real, billable `claude` unless you point it at a
> stand-in first.

For manual testing, use the bundled `mock-claude` stand-in:

```
CHOCOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude ./target/debug/chocofactoryd
```

`mock-claude` echoes back whatever it's sent (`echo:{text}`); set
`MOCK_CLAUDE_REPLY=<text>` to get a fixed reply instead. Point
`CHOCOFACTORY_CLAUDE_BINARY` at the real `claude` binary only when you
specifically mean to exercise the real CLI.

The daemon stores its database and workflow definitions under
`~/.config/chocofactory/`. On first start it seeds the built-in `chat`
workflow into `~/.config/chocofactory/workflows/` (existing files are never
overwritten). To keep a test run fully isolated from your real state,
override `HOME`:

```
HOME=$(mktemp -d) CHOCOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude \
  ./target/debug/chocofactoryd
```

### Daemon environment variables

| Variable | Purpose |
|---|---|
| `CHOCOFACTORY_CLAUDE_BINARY` | Path to the agent CLI. Unset = the real, billable `claude`. |
| `CHOCOFACTORY_CHOCO_BINARY` | Path to `choco`, used to serve every agent turn's `report_outcome` tool (see below). Unset = the daemon's own sibling `choco` binary. |
| `CHOCOFACTORY_PORT` | Bind port. Defaults to `4141`. Useful when a daemon is already running there. |
| `MOCK_CLAUDE_REPLY` | Read by `mock-claude` only — reply with this fixed text instead of echoing. |
| `MOCK_CLAUDE_REPORT` | Read by `mock-claude` only — the JSON input of the `report_outcome` call a single-shot turn makes (default `{"outcome": "done"}`). |
| `RUST_LOG` | Log filter, e.g. `error` to quiet startup, `debug` for detail. |

### Writing a workflow: how a stage routes on an agent's verdict

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
forgivingly — `## Findings`, `**Findings**` and `Findings:` are the same
thing, and `->` and `→` are interchangeable — and the list is generated
into the tool's own schema, so again there is no second copy in a prompt
file. A report that leaves a section out is rejected with an error the
agent can act on and call again.

This is how a verdict is kept from being cheaper than the work behind it: a
reviewer that stops at its first blocking finding has no walk to write down
(#95). Because parking a turn costs a human, the rule bends before it
breaks — after two rejections the report is recorded as it stands, with the
missing sections named in the tool's reply on the task's timeline.

That's because the CLI's end-of-turn line doesn't mean the work is done: an
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

### Writing a workflow: what an agent inherits from your Claude setup

By default an agent role runs isolated from the operator's own Claude Code
setup: no `~/.claude/CLAUDE.md`, no user plugins, hooks or output style, no
MCP servers other than the daemon's, no skills, no auto-memory, and no
built-in `ReportFindings` tool. The task repo's own `CLAUDE.md` and
`.claude/settings.json` still apply, including any hooks or plugins that
repo enables: they belong to the code being worked on. A role can loosen the
rest in the workflow file:

```yaml
roles:
  coder:
    skills: [run-tests]   # skills it may invoke; omitted = none
    memory: true          # use auto-memory; omitted = no
  chat:
    inherit_operator_config: true   # your full setup, as before
```

`skills`/`memory` can't be combined with `inherit_operator_config`. None of
these can be set from task config (`--config`, `--role-*`) or the global
config file: only a workflow definition can loosen what its agents see. The
built-in `chat` workflow inherits your setup; `coding-task` is isolated. The
daemon never overwrites a workflow already seeded into
`~/.config/chocofactory/workflows/`, so an existing `chat.yaml` there needs
`inherit_operator_config: true` added by hand to keep its old behaviour.

Each session's `session_meta` event records what it actually ran with: the
CLI version, model, tools, MCP servers, plugins, skills, and the isolation
it was launched under.

### Reviewing a `coding-task` PR

When a `coding-task` reaches `awaiting_human_review` it has already pushed
a branch, opened a PR and waited for CI. What it wants from you is a
verdict — and it reads that from the PR's **comments**, not from GitHub's
formal review (the green *Review changes* button).

That is deliberate rather than a shortcut. `open_pr` pushes under whatever
identity the daemon inherited, so on a solo repo the PR belongs to the same
account that would review it, and GitHub refuses a formal review from a
PR's own author:

```
failed to create review: GraphQL: Review Can not request changes on your
own pull request (addPullRequestReview)
```

Commenting on your own PR is allowed, so the verdict lives in a comment.
Leave an ordinary PR comment containing one of these markers, **alone on
its own line**:

| Marker             | Effect                          |
| ------------------ | ------------------------------- |
| `/approve`         | the task moves to `done`        |
| `/request-changes` | the task goes back to `revising` |

The rest of the comment is yours to write however you like — put the marker
on the last line and your review above it. A comment that reads "Two
findings, one worth fixing before merge." followed by your prose, and then
a final line containing only `/request-changes`, sends the coder back round
with your review already on the PR for it to read.

Five things worth knowing:

- **Only comments newer than the newest commit count.** Once the coder
  pushes a fix your previous verdict stops counting on its own, so there is
  nothing to clear between rounds. The flip side: if a `revising` lap ends
  without producing a commit, your old verdict is still the newest thing on
  the PR and will be read again.
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
- **Only people with standing in the repo can vote.** A comment counts
  only if GitHub reports its author as `OWNER`, `MEMBER` or `COLLABORATOR`
  — this repo is public, so without that fence any passer-by could
  `/approve` a task to `done`, or burn a coder+reviewer lap at a time with
  `/request-changes`. Comments from `[bot]` accounts are skipped on top of
  that, so a CI reviewer is never mistaken for your verdict. Neither fence
  distinguishes *you* from an agent acting as you: anything commenting
  under your account counts as you.

If no verdict arrives within six hours the task stops waiting and parks at
`escalate_to_human`, where `choco task send <id> "<note>"` resumes it into
`revising`. Three `/request-changes` rounds park it the same way instead of
looping.

## Using the `choco` CLI

With a daemon running, in a second shell:

```
choco [--base-url <url>] <COMMAND>
```

The base URL defaults to `http://127.0.0.1:4141`, and can also be set via
the `CHOCO_BASE_URL` environment variable.

Commands print a human-readable summary by default. Pass `--json` to get
the daemon's raw JSON instead — `choco` is meant to be both human-scriptable
and agent-callable, and `--json` is the half you pipe into `jq` or parse
from an agent. On failure it prints `error: <message>` to stderr and exits
`1`.

### A full walkthrough

Create a project:

```
$ choco project create acme
Name     acme
ID       7a0cafdf-8c3a-4e9f-8453-78d11be2a4e4
Created  2026-08-01 12:33:37 UTC
```

Create a task in it. `--project` takes **either the project name or its
id** — a name is resolved against `project list`, and is rejected naming
the candidates if it matches more than one project (names aren't unique).
`--workflow` names any definition in `~/.config/chocofactory/workflows/`
(`chat` ships built in):

```
$ choco task create --project acme --workflow gated \
    --title "ship the thing" --prompt "start"
Title     ship the thing
ID        bb93ada3-2910-4b94-911d-f6e8aab426dd
Project   7a0cafdf-8c3a-4e9f-8453-78d11be2a4e4
Workflow  gated
Status    open
Created   2026-08-01 12:33:37 UTC
```

Check where it is. `Progress` shows the stages the task has passed
through, the outcome that caused each hop, and when it happened —
starting with the stage it began in:

```
$ choco task status bb93ada3-...
Title     ship the thing
...
Stage     review

Progress
  1. gate (start)   2026-08-01 12:33:31 UTC
  2. gate --[resumed]--> review   2026-08-01 12:33:37 UTC   (current)
```

The trail comes from the task's `stage_entered` events, so the same
transitions also show up inline in `choco task events` alongside the
conversation. A task whose history has aged out of retention still gets
its current stage named on a trailing line.

A task with no recorded transitions at all says so, rather than showing
a blank list:

```
Progress
  → gate (current, no transitions yet)
```

Send a message into the task's live session (or resume a `human_gate`).
The daemon accepts it asynchronously — the agent's reply lands as an
event, not in this response:

```
$ choco task send bb93ada3-... --text "go"
Message accepted for task bb93ada3-.... The reply is recorded as an event
— see `choco task events bb93ada3-...`.
```

Stop a task that has gone wrong. This kills its agent process — and
anything that process started, like a test run or a dev server — marks the
task `cancelled`, and removes its worktree:

```
$ choco task cancel bb93ada3-...
Task bb93ada3-... cancelled. Any running agent process and worktree have
been cleaned up — see `choco task status bb93ada3-...`.
```

Cancelling ends the task's *work*, not its record: its events and the
stage it stopped in stay readable, which is the point of cancelling rather
than deleting. It can't be undone — a cancelled task accepts no further
messages, and cancelling one twice (or cancelling a task that already
finished) is a `409`.

### Stuck tasks

Sometimes the engine itself can't move a task forward — a stage's outcome
has no `on:` edge to route through, a transition failed, an agent turn's
session never started, a run was force-closed before it finished, an agent
never reported its outcome, or an agent's process kept running after its turn
ended. When
that happens the task is marked `stuck` rather than silently staying
`open`, with a human-readable reason attached.

Find them:

```
$ choco task list --status stuck
```

Read the reason:

```
$ choco task status bb93ada3-...
Title   t
ID      bb93ada3-...
...
Status  stuck
Stuck   stage 'run': command finished with outcome 'error' but the stage
        has no 'on:' edge for it
...
```

Recover with a retry, which re-enters the task's current stage — not a
replay of whatever happened before, since the daemon never persisted an
outcome to replay:

```
$ choco task retry bb93ada3-...
Retrying stage 'run' from scratch, in a fresh session: it is not an agent
turn, so it has no session. See `choco task status bb93ada3-...`.
```

A `shell` stage has no agent session, so there is nothing to resume and it
says so. An agent turn that was cut off from *outside* — the account hitting
a usage limit, or the daemon closing a session that had gone idle — is
resumed instead, continuing the same CLI session rather than starting a new
one over a working tree full of work it knows nothing about:

```
$ choco task retry bb93ada3-...
Retrying stage 'coding' by resuming its interrupted session
(47a18986-...) — it picks up where it left off, with its working tree
untouched. See `choco task status bb93ada3-...`.
```

Anything the agent itself got wrong still starts fresh — `Retrying stage
'coding' from scratch, in a fresh session: its turn ended 'no_report', which
is the agent's own failure rather than an interruption.` — so a turn that
crashes deterministically isn't resumed back into the same crash. Use
`--resume` to insist (it fails, rather than quietly starting fresh, when
there is nothing safe to resume) or `--fresh` to start over anyway.

Or give up on it the same way as any other task:

```
$ choco task cancel bb93ada3-...
```

A stuck task accepts no messages (`choco task send` is a `409`, the same
shape as sending to a cancelled task) until a retry reopens it.

Read the conversation:

```
$ choco task events bb93ada3-...
TIME                     KIND               DETAIL
2026-08-01 12:33:51 UTC  human_message      explain the plan
2026-08-01 12:33:51 UTC  session_meta       a4cbce43-e70c-49ab-a407-2ae4701b7838
2026-08-01 12:33:51 UTC  assistant_message  echo:explain the plan
```

Long output is paginated — pass `--limit N`, and follow the `--after
<token>` hint printed when more events remain. There is also a live
WebSocket stream at `/tasks/{id}/events/live` that the CLI doesn't wrap.

List things:

```
$ choco task list
TITLE      ID                                    STATUS  WORKFLOW  CREATED
chat task  ed9e8a7d-e5d4-4aeb-b04c-b47d14145940  open    chat      2026-08-01 12:33:51 UTC

$ choco task list --project acme          # by name or id
$ choco task list --status open           # free-form, not a fixed enum
$ choco task list --status cancelled      # open | closed | cancelled | stuck today
$ choco project list
```

### Scripting it

`--json` turns any command into machine-readable output:

```
$ choco --json task list | jq -r '.[0].id'
ed9e8a7d-e5d4-4aeb-b04c-b47d14145940

$ choco --json task status <id> | jq -r '.workflow_state.current_stage'
review
```

`task send` returns 202 with no body, so under `--json` it prints nothing
at all rather than a message that would break a pipe.

### Per-role config

A workflow can declare more than one role — a `coder` and a `reviewer`, say —
and each resolves its own CLI, model and system prompt from three layers,
most specific wins, independently per field:

```
task config (--role-* below)  >  the workflow's roles: block  >  ~/.config/chocofactory/config.yaml
```

The `--role-*` flags set the task-level layer. Each is `ROLE=VALUE` and each
is repeatable, so several roles can be configured in one command. Using a
two-role workflow of your own under `~/.config/chocofactory/workflows/` (the
built-in multi-role `coding-task.yaml` is still to come):

```
$ choco task create --project acme --workflow my-coding-task \
    --title "fix the flaky test" --prompt "see issue 41" --repo ~/src/acme \
    --role-model coder=opus \
    --role-model reviewer=sonnet \
    --role-system-prompt-file reviewer=./strict-reviewer.md
```

The role names are whatever that workflow's `roles:` block declares — a name
that isn't in it is simply not applied to anything.

| Flag | Sets |
|---|---|
| `--role-cli ROLE=CLI` | which agent CLI that role runs |
| `--role-model ROLE=MODEL` | that role's model |
| `--role-system-prompt ROLE=TEXT` | that role's system prompt, inline |
| `--role-system-prompt-file ROLE=PATH` | the same, read from a file |

There is deliberately no bare `--model`: with two roles it would be
ambiguous which one it meant.

`--role-system-prompt-file` is read by `choco` itself and sent as text — the
daemon is never handed a path from task config, which is the least-trusted
of the three layers.

`--config '<json>'` is the escape hatch, applied *before* the typed flags
(which win per field), for agent callers and for anything the flags don't
cover:

```
$ choco task create ... --config '{"roles":{"coder":{"model":"opus"}}}'
```

### Changing a task's config later

`task reconfigure` merges into a task's existing config, so changing one
role leaves the task-wide `--repo` and every other role alone:

```
$ choco task reconfigure <task-id> --role-model coder=haiku
```

It takes effect on the task's **next** turn: role config is re-read from the
database on every stage entry and never cached, so a session already running
keeps the config it started with.

### Other flags

- `--repo <path>` on `task create` sets the working directory for the
  task's agent subprocess (stored as `config.cwd`). Defaults to the
  daemon's own working directory.
- `--base-url <url>` targets a daemon on a non-default port, e.g. one
  started with `CHOCOFACTORY_PORT=41500`.

## Tests

```
cargo build --workspace --all-targets   # test harnesses spawn these binaries
cargo test --workspace
```

Tests never spawn the real `claude` — the integration suites point the
daemon at `mock-claude` or a Python fixture instead.
