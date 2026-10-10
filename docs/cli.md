# The `choco` CLI

This is the reference for the `choco` command line and the daemon it talks to: global flags, running and updating the daemon, a full walkthrough of a task's life, recovering stuck tasks, scripting, watching tasks, and the dashboard. Back to the [README](../README.md).

## Global flags, JSON and errors

```
choco [--base-url <url>] [--json] <COMMAND>
```

Commands print a human-readable summary by default. Pass `--json` to get
the daemon's raw JSON instead — `choco` is meant to be both human-scriptable
and agent-callable, and `--json` is the half you pipe into `jq` or parse
from an agent. On failure it prints `error: <message>` to stderr and exits
`1`.

`choco --version` (or `-V`) prints the CLI's version without contacting the
daemon. `chocofactoryd --version` prints the daemon's version and exits
without touching `$HOME`.

### Which daemon `choco` talks to (the base URL)

`choco` picks the daemon's address in this order:

1. `--base-url <url>` or the `CHOCO_BASE_URL` environment variable, if set;
2. otherwise the port of the running daemon, read from its lock file
   (`~/.config/chocofactory/chocofactoryd.lock`);
3. otherwise `http://127.0.0.1:4141`, but only when there is no lock file.

A lock file that records a dead daemon, or an empty one left by a failed
start, is an error (`chocofactoryd is not running`), never a fallback. A lock
file that exists but can't be read falls back to `http://127.0.0.1:4141` and
prints a warning on stderr. The `choco server` commands always use the lock
file. `choco` also warns on stderr when the daemon's version differs from its
own.

## The daemon

`chocofactoryd` is the daemon: it stores everything in a SQLite database and serves an HTTP/WebSocket API that `choco` talks to. Use `choco server` to run it. It expects `chocofactoryd` next to the `choco` binary:

```
choco server start [--port N]      # background, own session; waits until it answers
choco server stop [--force]        # graceful stop (SIGTERM)
choco server restart [--force] [--port N]   # keeps the port unless --port is given
choco server status                # version, pid, port, uptime, open tasks, in-flight work
```

- **Log:** `~/.config/chocofactory/logs/chocofactoryd.log`. At start, a log over
  10 MiB is moved to `chocofactoryd.log.1` (replacing any older one).
- **Exit codes:** `0` ok; `1` error; `3` means "not running" for `status`, and
  "refused" for `stop`/`restart`. `restart` also exits `1` when the old daemon
  had to be killed after 30 s; a new one is started only if the old one's lock was released.
- **Stop and running tasks:** agent turns and shell steps are killed and the
  tasks marked `stuck` (`choco task retry <id>` continues them; an agent turn
  resumes its session). Tasks waiting on a poll or a human are not affected.
  Without `--force`, `stop` refuses (exit 3) and lists in-flight work instead.

**One daemon per config directory.** It holds a lock on
`~/.config/chocofactory/chocofactoryd.lock` for as long as it runs and writes
a JSON description of itself there: `pid`, `port`, `version`, `commit`,
`started_at` and `exe`. A second daemon on the same `$HOME` (even on another
port) refuses to start and names the first one's pid and port. Never delete
the lock file: the operating system releases the lock when the daemon dies,
even by `kill -9`, so a leftover file from a dead daemon is harmless.

SIGTERM and Ctrl-C stop the daemon cleanly: every agent and shell process it
started is stopped with it. Tasks that were in the middle of an agent turn or
a shell command become `stuck`, and `choco task retry` continues them (an
agent turn resumes its session when it can; a shell command runs again from
the start). Waits that live in the database survive a restart untouched:
poll stages, human gates, and standing chat sessions. The same parking
happens at the next start if the daemon was killed hard.

The daemon keeps its database under `~/.config/chocofactory/`. The built-in
workflows (`chat`, `coding-task`, `coding-task-planned`) come from the daemon
binary, so upgrading the binary upgrades them.

Three environment variables are worth knowing as a user:

| Variable | Purpose |
|---|---|
| `CHOCOFACTORY_PORT` | Bind port. Defaults to `4141`. Useful when a daemon is already running there. `0` picks a free port; the bound port is written to the lock file. |
| `CHOCOFACTORY_OMP_BINARY` | Path to the `omp` CLI used by roles with `cli: omp`. Unset = `omp` from `PATH`. |
| `RUST_LOG` | Log filter, e.g. `error` to quiet startup, `debug` for detail. |

The full list of daemon variables, and how to run the daemon by hand, is in
[CONTRIBUTING.md](../CONTRIBUTING.md).

## Updating

```
choco update [--check] [--version X.Y.Z] [--force]
```

Works for copies installed by `install.sh` (it refuses, with instructions, for
source builds and cargo installs). `--check` only reports whether an update is
available. A daemon running from the install directory is stopped and restarted
on the same port with the new binary; if an agent turn or shell step is in
flight, `update` refuses (exit 3) unless `--force`, which parks that work like
`choco server stop --force` (`choco task retry` continues it). A daemon running
from another directory is left alone. The built-in workflows are inside the
binary, so they update with it.

The installer (`install.sh`) reads these environment variables:

- `CHOCO_INSTALL_DIR` — where to install (default `~/.local/bin`).
- `CHOCO_VERSION=X.Y.Z` — pin a version.
- `CHOCO_RELEASES_URL` — point at another release host.
- `CHOCO_INSTALL_ARCHIVE` — install from a local archive without downloading.

Downloads are verified against the release's `SHA256SUMS` before anything is
installed. Platforms: macOS arm64 and x86_64, Linux x86_64 and aarch64 (static
musl builds). The two binaries always live **side by side** in one directory:
`chocofactoryd` hands agents the `choco` next to it, and `choco server start`
runs the `chocofactoryd` next to it.

**From source:** `cargo install --git https://github.com/itsypkin/ChocoFactory --locked choco chocofactoryd`,
or clone and `cargo build --workspace` (see [CONTRIBUTING.md](../CONTRIBUTING.md)).

## A full walkthrough

Create a project:

```
$ choco project create acme --repo ~/code/acme
Name     acme
ID       7a0cafdf-8c3a-4e9f-8453-78d11be2a4e4
Repo     /Users/you/code/acme
Created  2026-08-01 12:33:37 UTC
```

`--repo` is optional on a project (see
[Project workflows](workflows.md#project-workflows)); the coding workflows need
one, from the project or from the task.

Create a task in it. `--project` takes **either the project name or its
id** — a name is resolved against `project list`, and is rejected naming
the candidates if it matches more than one project (names aren't unique).
`--workflow` is a workflow name (the project's own repo first, then the
built-ins — `chat`, `coding-task` and `coding-task-planned` ship in the daemon) or a path to a
workflow `.yaml` file — see [Project workflows](workflows.md#project-workflows):

```
$ choco task create --project acme --workflow coding-task \
    --title "ship the thing" --prompt "$(cat spec.md)"
Title     ship the thing
ID        bb93ada3-2910-4b94-911d-f6e8aab426dd
Project   7a0cafdf-8c3a-4e9f-8453-78d11be2a4e4
Workflow  coding-task
Status    open
Repo      /Users/you/code/acme
Created   2026-08-01 12:33:37 UTC
```

Check where it is. `Progress` shows the stages the task has passed
through, the outcome that caused each hop, and when it happened —
starting with the stage it began in:

```
$ choco task status bb93ada3-...
Title     ship the thing
...
Stage     internal_review

Progress
  #  from    outcome  to               at (UTC)
  1          start    coding           2026-08-01 12:33:31
  2  coding  done     internal_review  2026-08-01 12:33:37  ◀ current
```

Times are UTC. A step from today shows only the time (`12:33:37`); an
earlier day's step also shows its date, as above.

The trail comes from the task's `stage_entered` events, so the same
transitions also show up inline in `choco task events` alongside the
conversation. A task whose history has aged out of retention still gets
its current stage named: the table ends with a `→` row marked `◀ current`.

A task with no recorded transitions at all says so, rather than showing
a blank list:

```
Progress
  → coding (current, no transitions yet)
```

**Parallel groups.** A task whose current stage is a parallel group shows a
`Branches` table between the fields and `Progress`, one row per branch:
`branch`, `kind`, `state`, `result or reason` (the result of a `done` branch,
the reason of a `failed` one, cut to 60 characters), `time` and `cost`. `time`
is how long a branch ran, or has run so far while the task is open; `-` means
the branch is not running (the task is not open) or a time is missing. `cost`
is the branch's turn cost for the group's current entry; `no data` means no
turn recorded usage for it. `choco --json task status <id>` carries the full
text under `workflow_state.branches`. The table is gone once the task leaves
the group.

```
Branches
  branch               kind        state    result or reason                                              time  cost
  security_review      agent_turn  running                                                                5m    ≈ $0.42
  architecture_review  agent_turn  done     blocking                                                      2m    cost unknown
  tests                ?           failed   aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbb…  3m    no data
```

**Cost and time.** After the progress list, `choco task status` prints what
the task has used, recorded from every agent turn's own report:

```
Cost & time
  Total        ≈ $0.09 (API-equivalent)
  Tokens       input 30 · output 15 · cache read 300 · cache write 60
  Wall time    2h05m
  Active time  1h10m
  By stage
    coding         ≈ $0.05  (in 20 · out 10 · cache read 200 · cache write 40)
    internal_review no data
  By role
    coder          ≈ $0.05  (…)
  By lap
    coding #1      ≈ $0.03  (…)
    coding #2      ≈ $0.02  (…)
  By model
    claude-sonnet  ≈ $0.09  (…)
```

The cost is the CLI's own list-price figure, so it is always approximate (`≈`).
It reads `(API-equivalent)` when every turn ran under a subscription login —
what the same usage would cost on the API — and `(estimated)` otherwise (an API
key, a mix, or a CLI that does not say). Money has two decimals. A figure the
CLI did not report prints `?` (tokens), `cost unknown` (cost) or `no data`;
it is never counted as zero. The total line ends `(N sessions without data)` when
sessions ended without reporting a turn, such as a killed one, and
`(N turns without a cost)` when a turn reported tokens but no cost, so the total
is a lower bound; the dashboard's detail row carries the same notes, and a
list cell with such turns ends in `+`. A turn's tokens are the sum of its
per-model figures when the CLI reports them, which includes sub-agent models.
For an omp role the turn's figures are omp's own session statistics, which
include its sub-agents and side calls but not a response omp discarded, and the
per-model split adds up to them. A task created by an older version, before
usage was recorded, prints the single line `Cost & time  no data`.

*Wall time* runs from creation to now while the task is open or stuck, and to its
last update after that. *Active time* adds up the stages the task spent working,
leaving out time at a `human_gate` and in a `terminal` stage; it comes from the
stage trail, so it reads `no data` once that trail has aged out of retention.
Cost and tokens are kept for good. A *lap* is the nth time the task entered a
stage; a retry stays in its lap. `choco task list --json` carries each task's
total as `usage_total`.

Send a message to a task. What a task accepts depends on its stage: a
standing agent stage such as `chat`'s takes a message into the live session
(the agent's reply is recorded as an event), and a `human_gate` (or
`escalate_to_human`) takes a message that resumes it.
A `coding-task` accepts a message only at its human gates; at its other agent
stages (`coding`, `internal_review`, `revising`) the daemon answers `409`.
At a gate that has markers, such as `awaiting_human_review`, the text must
carry exactly one of them on a line of its own.

Once the task reaches `awaiting_human_review`, a message with a marker is
recorded as an event and moves the task on (here `/approve` moves it to done).
The daemon accepts it asynchronously, so the effect shows up in the task's
events, not in this response:

```
$ choco task send bb93ada3-... --text $'Looks good.\n/approve'
Message accepted for task bb93ada3-.... The reply is recorded as an event
— see `choco task events bb93ada3-...`.
```

Stop a task that has gone wrong. This kills its agent process — and
anything that process started, like a test run or a dev server — marks the
task `cancelled`, and removes its worktree and its local branch
(`task/<id>`), pushed or not. The branch's tip commit is written to the
task's timeline before the branch is deleted, so `choco task events` still
tells you where it was:

```
$ choco task cancel bb93ada3-...
Task bb93ada3-... cancelled. Any running agent process, worktree and local
branch have been cleaned up — see `choco task status bb93ada3-...`.
```

To take the work over yourself, cancel with `--keep`. It stops the agents
and marks the task cancelled but keeps **both** the worktree and the
branch, and hands them to you. Anything re-entering the task later would
collide with them. `choco task status` then shows the kept worktree path
and branch (and `--json` carries `kept_work: true`).

A task that reaches `done` also removes its worktree, and deletes its
branch when the work is safe elsewhere: the branch tip is on a
remote-tracking ref, so it was pushed or is already merged into a fetched
`origin/main`. This is decided from local refs only, with no fetch. An
unpushed branch is kept, and the timeline says why. Remote branches are
never deleted by the daemon, and branches of tasks finished by older
versions are left alone.

Cancelling ends the task's *work*, not its record: its events and the
stage it stopped in stay readable, which is the point of cancelling rather
than deleting. It can't be undone — a cancelled task accepts no further
messages, and cancelling one twice (or cancelling a task that already
finished) is a `409`.

## Stuck tasks

Sometimes the engine itself can't move a task forward — a stage's outcome
has no `on:` edge to route through, a transition failed, an agent turn's
session never started, a run was force-closed before it finished, an agent
never reported its outcome, an agent's process kept running after its turn
ended, or a read-only role changed its worktree. When
that happens the task is marked `stuck` rather than silently staying
`open`, with a human-readable reason attached.

Find them:

```
$ choco task list --status stuck
```

Read the reason. This example is a workflow with a shell stage named `run`:

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

A task that is not stuck but waits at a gate because a watcher timed out (the
PR review watch, CI polling) can be retried too. It goes back to the watcher
with its schedule starting over, and no agent lap is spent. `--resume` and
`--fresh` don't apply and are refused:

```
$ choco task retry bb93ada3-...
Watching again: back to stage 'awaiting_human_review' (its schedule starts
over). See `choco task status bb93ada3-...`.
```

`choco task status` says when this applies. Any other open task can't be
retried.

Or give up on it the same way as any other task:

```
$ choco task cancel bb93ada3-...
```

A stuck task accepts no messages (`choco task send` is a `409`, the same
shape as sending to a cancelled task) until a retry reopens it.

Read the conversation (this sample is from a `chat` task):

```
$ choco task events bb93ada3-...
TIME                     KIND               DETAIL
2026-08-01 12:33:51 UTC  human_message      explain the plan
2026-08-01 12:33:51 UTC  session_meta       a4cbce43-e70c-49ab-a407-2ae4701b7838
2026-08-01 12:33:51 UTC  assistant_message  echo:explain the plan
```

Long output is paginated — pass `--limit N`, and follow the `--after
<token>` hint printed when more events remain. `--after` takes the
`next_token` field of `--json` output or the token from the "More events
available" hint. There is also a live
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

## Scripting it

`--json` turns any command into machine-readable output:

```
$ choco --json task list | jq -r '.[0].id'
ed9e8a7d-e5d4-4aeb-b04c-b47d14145940

$ choco --json task status <id> | jq -r '.workflow_state.current_stage'
internal_review
```

`task send` returns 202 with no body, so under `--json` it prints nothing
at all rather than a message that would break a pipe.

## Watching a task

`choco task status <id>` can watch a task or wait on it, so scripts need no
hand-written polling loop. It polls the daemon (`GET /tasks/<id>`).

| Flag | Meaning |
|---|---|
| `--live` | Keep the view current until the task closes or is cancelled. |
| `--until <target>` | Block until the task reaches `<target>`, then exit 0. Implies watching. |
| `--interval <dur>` | Poll cadence. Default `2s`. Needs `--live` or `--until`. |
| `--timeout <dur>` | Give up after this long and exit 5. Default: none. Needs `--live` or `--until`. |

`<dur>` is `<integer><s|m|h>`, non-zero, the same spelling as workflow YAML
(`5s`, `30s`, `5m`, `1h`). `<target>` is `attention`, `closed`, `cancelled`,
`stuck`, or `stage:<name>` (the task has entered that stage, even if it
already left it between two polls). `attention` means the task needs you: it
is open at any `human_gate` stage (whatever the stage is called), or it is
stuck, cancelled or closed. It looks at the current state only, so a watch
started while the task already sits at a gate returns at once. The `stage:` prefix keeps stage names apart from
statuses. A `--live` watch alone does not stop at `stuck`, since a human may
`choco task retry` it.

| Exit code | Meaning |
|---|---|
| 0 | The target was reached (with `--live` alone: the task closed). |
| 1 | An error: unknown task, API error, or the daemon is unreachable (a connection lost mid-watch is retried; three failures in a row end it). |
| 2 | A usage error. |
| 3 | The task became stuck, and `stuck` was not the target. |
| 4 | The task was cancelled, and `cancelled` was not the target. |
| 5 | `--timeout` elapsed first. |
| 6 | The task closed without reaching the target. |

Under `--until attention` the codes mean: 0 the task is open at a
`human_gate` stage (stderr says which stage); 3 stuck; 4 cancelled; 6 closed.

Every non-zero exit from a watch explains itself on stderr.

```
$ choco task status "$id" --until closed --timeout 2h
$ case $? in
    0) echo "done" ;;
    3) echo "stuck — needs a human" ;;
    4) echo "cancelled" ;;
    5) echo "still running after 2h" ;;
    *) echo "something else went wrong" ;;
  esac
```

To wait for whatever stops the task next, use `attention`. The line on
stderr names the stage:

```
$ choco task status "$id" --until attention --timeout 2h
$ case $? in
    0) echo "waiting for you at a gate" ;;
    3) echo "stuck" ;;
    4) echo "cancelled" ;;
    5) echo "still running after 2h" ;;
    6) echo "closed" ;;
  esac
```

Watching in a terminal redraws the status view every poll:

```
$ choco task status "$id" --live
```

When stdout is piped, the output is one line per change, with no escape
sequences. With `--json` it is NDJSON: one full task object per line, per
change.

## The dashboard

`choco dashboard` (alias `choco dash`) is an interactive terminal view of every
task on the daemon. It needs a terminal: with a pipe, a redirect or `--json` it
exits 1 and points to `choco task list` and `choco task status --live`.

```
$ choco dashboard [--project <name|id>] [--interval <dur>] [--closed <N>]
```

`--interval` is how often it polls (default `2s`); `--closed` is how many
recently closed or cancelled tasks to show (default 10). The screen is one
scrollable list in four sections:

| Section | Holds | Ordered |
|---|---|---|
| Needs you | `open` tasks at a `human_gate`, in any workflow | longest waiting first |
| In progress | every other `open` task, with its laps (the largest loop counter, `×N`) | longest in its stage first |
| Stuck | `stuck` tasks, whatever their stage, with the first line of the reason | longest stuck first |
| Recently closed | the latest `closed` and `cancelled` tasks | newest first |

**Two modes.** Without `--project` every row has a `project` column (the
project's name) and the header says `all projects`. With `--project` the column
is gone and the header names the project. In a parallel group the stage column reads `<group> settled/total`, for
example `review_panel 2/3`. Narrow terminals drop columns: below
91 columns the `cost` column (the task's total, `≈$1.23`, or `no data`), below
80 the laps and PR columns, below 60 the stage and project columns too.
Below 40×10 it only says the terminal is too small. `NO_COLOR` turns colour off.

| Key | Action |
|---|---|
| `↑` `k` / `↓` `j` | move, across section boundaries |
| `PgUp` `PgDn`, `g` `G` | page; top / bottom |
| `Tab` / `Shift-Tab` | next / previous non-empty section |
| `⏎` | open the task's status view: the fields and progress `choco task status` shows (for a parallel group, its branch table too), the loop counters, what the task is waiting for, its PR, a `Cost` row (`≈ $0.09 (API-equivalent) · wall 2h05m · active 1h10m`, or `no data`) and the last 5 events, following new ones (`Esc` returns) |
| `e` | in the detail: the full event stream (the last 200 events, following new ones; `PgUp`/`PgDn` scroll back, `End` follows again); `e` or `Esc` returns to the status view |
| `o` | open the task's pull request |
| `r` | retry a `stuck` task |
| `c` | cancel an `open` or `stuck` task |
| `?` | list the keys |
| `q`, `Ctrl-C` | quit |

The selection follows the task, not the row, so it stays put when a refresh
re-sorts the list.

Actions apply to the selected task (the open one, in the detail view):

- `r` asks `Retry "<title>"? Its stage runs again. [y/N]`; `y` runs
  `choco task retry` (resuming the agent session when it can) and reports
  `retried: resumed` or `retried: fresh`. On a task that is not stuck it says so
  and sends nothing.
- `c` asks `Cancel "<title>"? This kills its agent and deletes its worktree and
  branch. It cannot be undone. [y/N]`; `y` cancels the task. The dashboard never
  keeps the work (`--keep` is `choco task cancel` only).
- Any key but `y` closes the question and sends nothing. A daemon error is shown
  verbatim on the bottom line, and a success refreshes the list at once.
- `o` opens the PR with `open` (macOS) or `xdg-open`. Over SSH (`SSH_CONNECTION`
  or `SSH_TTY` set) it launches nothing and prints `PR #N: <url>` instead.

If the daemon goes away the last data stays on screen, the bottom line turns red
(`daemon unreachable: …; retrying every 2s (data 34s old)`) and polling goes on,
so the dashboard recovers on its own.

## Per-role flags and changing a task's config

A workflow can declare more than one role — the built-in `coding-task` has a
`coder` and a `reviewer` — and each role resolves its own CLI, model and system
prompt. The `--role-*` flags on `choco task create` set the task-level layer.
Each is `ROLE=VALUE` and each is repeatable:

| Flag | Sets |
|---|---|
| `--role-cli ROLE=CLI` | which agent adapter runs that role (`claude` or `omp`) |
| `--role-model ROLE=MODEL` | that role's model |
| `--role-system-prompt ROLE=TEXT` | that role's system prompt, inline |
| `--role-system-prompt-file ROLE=PATH` | the same, read from a file |

`--config '<json>'` is the escape hatch for anything the flags don't cover.
The layers, the adapters, an example and the details are in
[models.md](models.md).

`task reconfigure` merges into a task's existing config, so changing one
role leaves the task-wide `--repo` and every other role alone:

```
$ choco task reconfigure <task-id> --role-model coder=haiku
```

It takes effect on the task's **next** turn: role config is re-read from the
database on every stage entry and never cached, so a session already running
keeps the config it started with.

## Other flags

- `--repo <path>` on `task create` sets the working directory for the
  task's agent subprocess (stored as `config.cwd`). Defaults to the project's
  repo when the project has one. With neither, creating a task on a worktree
  workflow (all the coding built-ins) fails with an error and nothing is
  created. A non-worktree workflow such as `chat` runs in the daemon's own
  working directory.
- `--base <ref>` on `task create` is the commit the task's worktree forks
  from: a `<remote>/<branch>` (fetched first), or any local branch, tag, SHA
  or `HEAD`, used as the repo has it. Without it, the task forks from the
  remote's default branch, freshly fetched (`origin`, or the only remote), or
  from the repo's HEAD when it has no remote. A ref that doesn't resolve, or
  a failed or timed-out fetch, creates nothing. Only valid for a worktree
  workflow. `choco task status` shows the result on its `Base` line.
- `--base-url <url>` targets a daemon on a non-default port, e.g. one
  started with `CHOCOFACTORY_PORT=41500`.
