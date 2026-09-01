# 04 — Implementation Plan

Based on the approved `03-design.md`. Tasks are grouped by the two phases
defined in §8. Each task lists its design references and dependencies on
other tasks in this plan. GitHub issue links are filled in after issue
creation (see bottom of file for the mapping).

## Phase 1 — Projects/Tasks core + Type 1 (chat) end-to-end

### P1-1. Workspace scaffolding

Set up a Cargo workspace with three crates: `chocofactoryd` (daemon: API
+ workflow engine + adapters), `choco` (thin CLI client), and
`chocofactory-core` (shared types used by both — `Event`, DB models,
workflow definition types). Add basic CI (build, test, clippy, fmt) via
GitHub Actions.

- Design ref: §2 Architecture
- Depends on: none

### P1-2. SQLite schema & migrations

Implement the schema for `projects`, `tasks`, `task_runs`, `events`,
`workflow_state` as described in §3, with a migration tool (e.g. `sqlx
migrate` or `refinery`) and a repository/DAO layer providing CRUD for
each table.

- Design ref: §3
- Depends on: P1-1

### P1-3. Agent adapter abstraction + Claude adapter

Define the `AgentAdapter` trait (§4) and implement `ClaudeAdapter`,
wrapping `claude --print --output-format=stream-json [--resume <id>]` as
a subprocess. Translate Claude Code's native stream-json events into the
shared `Event` enum (§4.2: `AssistantMessage`, `ToolCall`, `ToolResult`,
`Thinking`, `SessionMeta`, `Error`).

- Design ref: §4, §4.2
- Depends on: P1-1

### P1-4. Session lifecycle manager (active/idle/resume) + idle reaper

Implement the active ⇄ idle ⇄ resume state machine (§4.1) on top of
`task_runs`: live subprocess while active, teardown + `session_id`
persistence after an idle timeout, resume via a fresh process on the next
message. Add the background idle reaper (§4.3) that also handles daemon-
restart recovery — any `task_runs` row left `active` at daemon startup is
flipped to `idle` (its process is gone).

- Design ref: §4.1, §4.3
- Depends on: P1-2, P1-3

### P1-5. Event capture + retention job

Wire adapter-emitted events into the `events` table, append-only,
normalized per §4.2. Add a daily scheduled job that prunes `events` rows
older than 1 year (§4.4), leaving `tasks`/`task_runs` untouched.

- Design ref: §4.2, §4.4
- Depends on: P1-2, P1-3

### P1-6. Workflow definition loader

Parse workflow definition YAML files (§5.1) into an in-memory graph:
`roles`, `stages`, each stage's `kind` + config + `on:` map. Resolve
`prompt_file`/`system_prompt_file` paths relative to the definition
file's location. Validate at load time (every `on:` target names an
existing stage, at least one `terminal` stage is reachable, etc.) and
fail fast with a clear error on a malformed definition.

- Design ref: §5.1
- Depends on: P1-1

### P1-7. Workflow engine core (Phase-1 stage kinds)

Implement the generic stage/transition interpreter (§5) driving a task's
`workflow_state` (current stage, stage history). Phase 1 needs three
stage kinds: `agent_turn` (drives a role's turn via the adapter/session-
lifecycle machinery from P1-3/P1-4), `human_gate` (pauses, waits for a
human message, emits `resumed`), and `terminal`. Structure
`workflow_state` so `loop_guard` bookkeeping (§5.3, needed in Phase 2)
fits without a schema change later.

- Design ref: §5, §5.2, §5.3 (state shape only)
- Depends on: P1-2, P1-4, P1-6

### P1-8. Built-in chat workflow

Ship `workflows/chat.yaml` (§5.4): a single `agent_turn` stage, role
`chat`, `on: {}` — embedded into the `chocofactoryd` binary and seeded
(write-if-missing) into `~/.config/chocofactory/workflows/` on first run
(§2.2). Wire task creation for this workflow so the task's initial input
becomes the first message into the session, and all further messages are
fed into the same open stage. Implement role config resolution (§5.5,
Q8) scoped to a single role for now: global config → workflow-def
`roles:` block → task-level `config` override, all three keyed by role
name (e.g. `config.roles.chat.model`).

- Design ref: §5.4, §2.2
- Depends on: P1-7

### P1-9. HTTP/WS API layer

Implement `chocofactoryd`'s API: project CRUD; task create/list/status;
send-message (feeds the active session, or triggers resume per §4.1); a
WebSocket endpoint streaming a task's `events` live. Bind to `127.0.0.1`
only, no auth (Q15).

- Design ref: §6.1, §6.2
- Depends on: P1-2, P1-4, P1-5, P1-8

### P1-10. `choco` CLI

Implement the `choco` binary (§6.2) as a thin HTTP client against the
daemon: `task create`, `task status`, `task send`, `task list`, `project
create`/`list`. Support `--parent-task <id>` to tag `tasks.parent_task_id`
for delegation.

- Design ref: §6.2
- Depends on: P1-9

### P1-11. Web UI — navigation, live chat, event timeline

React + TS app, served as a static bundle by `chocofactoryd`: project
list → task list → task detail. Task detail has a live chat pane (WS) and
the full event timeline (§4.2), collapsed tool calls by default,
expandable. New-task flow: pick project, workflow definition, repo/
working dir, initial prompt.

- Design ref: §6.1
- Depends on: P1-9

## Phase 2 — Type 3 (coding task)

### P2-1. `shell` stage kind

Implement the `shell` stage kind (§5.2): run a one-shot `command` or
`script_file` to completion; exit code 0 → `done`, nonzero → `error`;
`capture: json|text` parses stdout into the stage's `workflow_state`
payload at `stages.<stage name>`.

The command runs detached and reports its outcome through `advance`,
carrying the capture into the same locked write as the transition. Adds
an optional per-stage `timeout:` (nothing else would ever reap a hung
command) and a `shell_output` timeline event, since a shell stage has no
`task_run` to hang events off. The runner lives in its own module so P2-2
can drive it on an interval.

- Design ref: §5.1, §5.2
- Depends on: P1-7

### P2-2. `poll` stage kind

Implement the `poll` stage kind (§5.2): run a command on `interval` up to
an optional `timeout`, matching output against an ordered `outcomes:`
list (substring/regex) to pick the outcome; `on_timeout` fires if
`timeout` elapses with no match.

The loader already parsed and validated `poll`, and P2-1 built `shell.rs`
as a reusable runner for exactly this, so the work is the loop, the
matching (`poll.rs`) and the timeline policy. Widens `poll` to take
`script_file:` and `capture:` like `shell`, sharing one `command:`
resolution between the two kinds. Exit codes decide nothing — only a
command that can't start ends the loop early; each attempt is capped at
the remaining budget; only decisive and changed-output attempts are
recorded; and the loop abandons itself if the task leaves the stage.

- Design ref: §5.1, §5.2
- Depends on: P1-7

### P2-3. Cross-stage templating

Implement `{{ stages.<name>.<field> }}` substitution into `command:` and
`prompt_file` rendering, reading from other stages' captured payloads in
`workflow_state` (§5.1, "Templating/capture across stages"). Scope
strictly to variable substitution — no conditionals/expressions (§7
non-goal).

- Design ref: §5.1
- Depends on: P2-1, P2-2

### P2-4. Loop guards

Implement `loop_guard` (§5.3): a per-stage, per-outcome counter that
reroutes to `then:` after `max` transitions through a given outcome;
reset when the stage is entered from a different prior stage than last
time.

- Design ref: §5.3
- Depends on: P1-7

### P2-5. Worktree manager

Implement git worktree lifecycle (§5.5, Q7): `git worktree add
../<project>-wt-<task-id> -b task/<task-id>` on first entry into a stage
needing a working copy; removal on reaching a `terminal` stage or task
cancellation.

- Design ref: §5.5
- Depends on: P1-2

### P2-6. Multi-role config resolution

Extend role config resolution (started in P1-8) to support multiple
named roles per workflow definition (`coder`, `reviewer`), each
independently resolving CLI/model/system-prompt through the same global →
workflow-def → task-level layering (§5.5, Q8).

- Design ref: §5.5
- Depends on: P1-8

### P2-7. Built-in coding-task workflow

Author `workflows/coding-task.yaml` (§5.1) and its prompt files
(`coder-system.md`, `reviewer-system.md`, `coder-turn.md`,
`reviewer-turn.md`) — same embed-and-seed treatment as `chat.yaml`
(§2.2, P1-8): the prompt files travel with the definition into the seed
step, not just the YAML, since `roles:`/stage `*_file` fields resolve
relative to wherever the seeded copy ends up on disk. Wiring the full
stage graph: `coding` →
`internal_review` (loop-guarded, escalates via `escalate_to_human`) →
`open_pr` → `checks_polling` → `awaiting_human_review` → `done`, using
the `shell`/`poll` kinds, templating, loop guards, and worktree manager
from the tasks above.

- Design ref: §5.1, §5.5
- Depends on: P2-1, P2-2, P2-3, P2-4, P2-5, P2-6

### P2-8. Task delegation end-to-end

Validate that an agent running inside a task can call `choco task create
--parent-task <id>` from within its subprocess environment and poll the
child task's status (§6.2), exercised against a real coding-task run
(P2-7) to confirm the composition story works, not just the chat case
from Phase 1.

- Design ref: §6.2
- Depends on: P1-10, P2-7

## Additive — not gated by phase

### X-1. ACP adapter spike

Prototype an `AcpAdapter` implementing the existing `AgentAdapter` trait
(§4) against the Agent Client Protocol instead of parsing a CLI's native
stream directly. Use the official `claude-code-acp` bridge as the first
target. Scope:

1. Spawn/speak to the bridge over JSON-RPC (`session/new`, `session/prompt`),
   and translate `session/update` notifications into `AgentEvent`s (§4.2,
   §4.5) — reuse the same enum `ClaudeAdapter` already targets, so this
   is a drop-in alternative, not a new event shape.
2. Validate `session/load`/`session/resume` actually satisfies §4.1's
   idle→resume cycle (close, persist session id, reopen on next message)
   the same way `--resume <id>` does today.
3. Note operational overhead (Node dependency, extra process hop) versus
   the current direct-subprocess approach.
4. Write up a go/no-go recommendation: adopt `AcpAdapter` as the primary
   Claude transport, keep both behind a config flag, or drop it and stay
   with direct `stream-json` parsing.

Implementing full production support (replacing/complementing
`ClaudeAdapter`, adding Codex/Gemini via the same `AcpAdapter`) is a
follow-up task scoped after this spike's findings — not pre-planned here
since it's conditional on the go/no-go call.

- Design ref: §4.5
- Depends on: P1-3 (needs the shipped `AgentAdapter` trait/`AgentEvent`
  enum to prototype against)

### X-2. Mock claude provider for e2e tests + safe manual smoke testing

Today's tests (`chocofactoryd/src/api/mod.rs`'s in-process `TestServer`)
already avoid the real `claude` CLI by pointing `ClaudeAdapter` at a
Python stand-in (`chocofactoryd/tests/fixtures/fake_claude*.py`) via
`ClaudeAdapter::with_binary`. Two gaps that leaves open: (1) there's no
true e2e test that drives the actual `chocofactoryd` binary (`main.rs`)
over real HTTP/WS — only the in-process axum router; `main.rs` hardcodes
`ClaudeAdapter::new()` with no override, so a real e2e run would hit the
real CLI, and (2) manual smoke-testing the daemon has the same problem —
already bit us once, spawning a real billable `claude --print` session
during P1-9 testing. Scope:

1. Add a `CHOCOFACTORY_CLAUDE_BINARY` env var read in `main.rs`: if set,
   `ClaudeAdapter::with_binary(path)`; otherwise `ClaudeAdapter::new()`
   (today's default, unchanged). This alone unblocks both (1) and (2)
   using the existing `with_binary` seam — no `AgentAdapter`/`AgentHandle`
   changes needed.
2. Promote the ad hoc Python fixtures into a first-class, supported mock:
   a small Rust binary (new workspace member, e.g. `mock-claude`) that
   speaks the same `--print --output-format=stream-json
   --input-format=stream-json [--resume <id>]` protocol `ClaudeAdapter`
   already targets. Rust over Python removes the interpreter/venv
   friction hit during manual testing (missing `websockets` module, etc.)
   and keeps it buildable via plain `cargo build` for CI/e2e use.
3. Reply behavior: default to echoing the input turn back (matches
   `fake_claude.py`'s current `echo:{text}` behavior, i.e. "reply
   whatever's in the prompt" per the original request), with an env var
   escape hatch (e.g. `MOCK_CLAUDE_REPLY`) for a fixed static reply when a
   test needs a deterministic, prompt-independent response. `--resume
   <id>` reuses the same `session_id` (§4.1), matching real `claude`'s
   resume contract closely enough for the idle/resume tests already in
   `session.rs`/`engine.rs`.
4. Add a true e2e test (new `chocofactoryd/tests/e2e_*.rs`): spawn the
   real `chocofactoryd` binary as a subprocess (`std::process::Command`,
   `CHOCOFACTORY_CLAUDE_BINARY` pointed at `mock-claude`, `HOME`/config
   root pointed at a temp dir), drive it over real HTTP + WS
   (`reqwest`/`tokio-tungstenite`, already dev-deps from P1-9), and tear
   it down — covering the full startup sequence (seeding, migrations,
   idle reaper, retention) that in-process `TestServer` skips.
5. Update the smoke-testing instructions (or a short `docs/`/README note)
   to point at `CHOCOFACTORY_CLAUDE_BINARY=/path/to/mock-claude` as the
   safe default for manual runs, real `claude` as an explicit opt-in.

Migrating the *existing* `TestServer`-based integration tests off the
Python fixtures onto `mock-claude` is a nice-to-have, not required —
they're free to keep using `fake_claude*.py` if the Rust binary doesn't
cover a scenario they need (e.g. `fake_claude_crash.py`'s abrupt-exit
case). A real multi-provider abstraction (Codex/Gemini `AgentAdapter`
implementations) is explicitly out of scope here — this task only proves
the *testing* seam works, same spirit as X-1 proving the *transport*
seam.

- Design ref: §4, §4.1
- Depends on: P1-9 (needs `main.rs`'s real startup sequence and the
  HTTP/WS surface to e2e-test against)

## GitHub issue mapping

Milestones: [Phase 1 — Chat MVP](https://github.com/itsypkin/ChocoFactory/milestone/1),
[Phase 2 — Coding Task Workflow](https://github.com/itsypkin/ChocoFactory/milestone/2)

| Task  | Issue |
|-------|-------|
| P1-1  | [#1](https://github.com/itsypkin/ChocoFactory/issues/1) |
| P1-2  | [#2](https://github.com/itsypkin/ChocoFactory/issues/2) |
| P1-3  | [#3](https://github.com/itsypkin/ChocoFactory/issues/3) |
| P1-4  | [#4](https://github.com/itsypkin/ChocoFactory/issues/4) |
| P1-5  | [#5](https://github.com/itsypkin/ChocoFactory/issues/5) |
| P1-6  | [#6](https://github.com/itsypkin/ChocoFactory/issues/6) |
| P1-7  | [#7](https://github.com/itsypkin/ChocoFactory/issues/7) |
| P1-8  | [#8](https://github.com/itsypkin/ChocoFactory/issues/8) |
| P1-9  | [#9](https://github.com/itsypkin/ChocoFactory/issues/9) |
| P1-10 | [#10](https://github.com/itsypkin/ChocoFactory/issues/10) |
| P1-11 | [#11](https://github.com/itsypkin/ChocoFactory/issues/11) |
| P2-1  | [#12](https://github.com/itsypkin/ChocoFactory/issues/12) |
| P2-2  | [#13](https://github.com/itsypkin/ChocoFactory/issues/13) |
| P2-3  | [#14](https://github.com/itsypkin/ChocoFactory/issues/14) |
| P2-4  | [#15](https://github.com/itsypkin/ChocoFactory/issues/15) |
| P2-5  | [#16](https://github.com/itsypkin/ChocoFactory/issues/16) |
| P2-6  | [#17](https://github.com/itsypkin/ChocoFactory/issues/17) |
| P2-7  | [#18](https://github.com/itsypkin/ChocoFactory/issues/18) |
| P2-8  | [#19](https://github.com/itsypkin/ChocoFactory/issues/19) |
| X-1   | [#32](https://github.com/itsypkin/ChocoFactory/issues/32) |
| X-2   | [#42](https://github.com/itsypkin/ChocoFactory/issues/42) |
