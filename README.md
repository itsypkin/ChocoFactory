# ChocoFactory

Two binaries:

- **`chokofactoryd`** — the daemon. Owns the SQLite database, the workflow
  engine, and an HTTP/WS API on `127.0.0.1:4141`.
- **`choco`** — a thin CLI client against that API (create/inspect/message
  tasks and projects).

## Build

```
cargo build --workspace
```

Binaries land in `target/debug/`.

## Running the daemon

> **`chokofactoryd` spawns the real `claude` CLI by default** — running the
> daemon will hit the real, billable `claude` unless you point it at a
> stand-in first.

For manual testing, use the bundled `mock-claude` stand-in:

```
CHOKOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude ./target/debug/chokofactoryd
```

`mock-claude` echoes back whatever it's sent (`echo:{text}`); set
`MOCK_CLAUDE_REPLY=<text>` to get a fixed reply instead. Point
`CHOKOFACTORY_CLAUDE_BINARY` at the real `claude` binary only when you
specifically mean to exercise the real CLI.

The daemon stores its database and workflow definitions under
`~/.config/chokofactory/`. On first start it seeds the built-in `chat`
workflow into `~/.config/chokofactory/workflows/` (existing files are never
overwritten). To keep a test run fully isolated from your real state,
override `HOME`:

```
HOME=$(mktemp -d) CHOKOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude \
  ./target/debug/chokofactoryd
```

### Daemon environment variables

| Variable | Purpose |
|---|---|
| `CHOKOFACTORY_CLAUDE_BINARY` | Path to the agent CLI. Unset = the real, billable `claude`. |
| `CHOKOFACTORY_PORT` | Bind port. Defaults to `4141`. Useful when a daemon is already running there. |
| `MOCK_CLAUDE_REPLY` | Read by `mock-claude` only — reply with this fixed text instead of echoing. |
| `RUST_LOG` | Log filter, e.g. `error` to quiet startup, `debug` for detail. |

## Using the `choco` CLI

With a daemon running, in a second shell:

```
choco [--base-url <url>] <COMMAND>
```

The base URL defaults to `http://127.0.0.1:4141`, and can also be set via
the `CHOCO_BASE_URL` environment variable. Every command prints compact
JSON to stdout on success; on failure it prints `error: <message>` to
stderr and exits `1`.

### A full walkthrough

Create a project — everything else hangs off a project id:

```
$ ./target/debug/choco project create demo
{"id":"6cd3fcff-...","name":"demo","created_at":"2026-08-01T11:55:07.497841029Z"}
```

Create a task in it. `--workflow` names any definition in
`~/.config/chokofactory/workflows/` (`chat` ships built in), and
`--title`/`--prompt` are both required:

```
$ ./target/debug/choco task create \
    --project 6cd3fcff-... \
    --workflow chat \
    --title "example task" \
    --prompt "hello there"
{"id":"34c9a1eb-...","project_id":"6cd3fcff-...","parent_task_id":null,
 "workflow_def":"chat","title":"example task","status":"open","config":{}, ...}
```

Check where it is. The useful field is `workflow_state.current_stage` —
top-level `status` is only ever `open`/`closed`:

```
$ ./target/debug/choco task status 34c9a1eb-...
{"id":"34c9a1eb-...","status":"open", ...,
 "workflow_state":{"current_stage":"chatting","stage_history":[],
                   "loop_counters":{},"payload":{}, ...}}
```

`stage_history` is the list of stages the task has already left, appended
on each transition — it's empty above only because this task hasn't
transitioned yet. A multi-stage workflow after one hop looks like:

```
"workflow_state":{"current_stage":"review","stage_history":["gate"], ...}
```

Send a follow-up message into the task's live session (or resume a
`human_gate`). The daemon accepts it asynchronously, so this prints
nothing and exits `0` — the agent's reply arrives as an *event*, not in
this response:

```
$ ./target/debug/choco task send 34c9a1eb-... --text "another message"
$ echo $?
0
```

List tasks, optionally filtered. Both filters are free-form strings, not
fixed enums:

```
$ ./target/debug/choco task list --project 6cd3fcff-...
$ ./target/debug/choco task list --status open
$ ./target/debug/choco project list
```

### Delegation

`--parent-task <id>` tags a new task as spawned from an existing one, so an
agent working inside task A can spin up task B and poll it:

```
$ ./target/debug/choco task create --project <p> --workflow chat \
    --title "subtask" --prompt "do the thing" --parent-task 34c9a1eb-...
{"id":"...","parent_task_id":"34c9a1eb-...", ...}
```

The parent id round-trips through `choco task status <child-id>`, and the
child can be polled with the same `task status` call any external script
would use.

### Other flags

- `--repo <path>` on `task create` sets the working directory for the
  task's agent subprocess (stored as `config.cwd`). Defaults to the
  daemon's own working directory.
- `--base-url <url>` targets a daemon on a non-default port, e.g. one
  started with `CHOKOFACTORY_PORT=41500`.

### Reading a task's output

Agent replies are recorded as events, which the CLI does not currently
wrap — `choco` covers task/project management only. To read them, hit the
daemon's endpoints directly:

```
$ curl -s http://127.0.0.1:4141/tasks/34c9a1eb-.../events | python3 -m json.tool
```

That returns a paginated `{"events": [...], "next_token": ...}` page
(`?limit=`/`?after=`), with `human_message` and `assistant_message` entries.
There is also a live WebSocket stream at `/tasks/{id}/events/live`.

## Tests

```
cargo build --workspace --all-targets   # test harnesses spawn these binaries
cargo test --workspace
```

Tests never spawn the real `claude` — the integration suites point the
daemon at `mock-claude` or a Python fixture instead.
