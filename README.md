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

The tool is present on *every* agent turn, not only ones that route — a
stage with no `on:` edges (or none matching `capture: json`) still offers
`report_outcome`, just with a free-form, non-routing `outcome`: useful for a
coder to note it's `blocked`, visible on the task's event timeline, but never
able to park a stage that has always advanced unconditionally. An agent that
never calls the tool still works: the engine falls back to parsing the
turn's final reply as JSON, same as before this existed.

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
$ choco task list --status cancelled      # open | closed | cancelled today
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

### Delegation

`--parent-task <id>` tags a new task as spawned from an existing one, so an
agent working inside task A can spin up task B and poll it:

```
$ choco task create --project acme --workflow chat \
    --title "subtask" --prompt "do the thing" --parent-task bb93ada3-...
Title        subtask
ID           e94b3293-c547-4dce-a31a-71dccffe8f3c
Project      7a0cafdf-8c3a-4e9f-8453-78d11be2a4e4
Workflow     chat
Status       open
Parent task  bb93ada3-2910-4b94-911d-f6e8aab426dd
Created      2026-08-01 12:33:37 UTC
```

The parent id round-trips through `choco task status <child-id>`, and the
child is polled with that same call — which is why the delegating agent
wants `--json`:

```
$ choco --json task status <child-id> | jq -r '.workflow_state.current_stage'
chatting
```

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
