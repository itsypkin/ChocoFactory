# 03 — Design

Based on `01-idea.md`. Research (Step 2) was skipped by explicit choice —
proceeding straight from idea to design.

## 1. Overview

ChokoFactory is a local tool for structuring how you work with coding
agents. Work is organized into **Projects** (logical groupings of related
work, not tied to a single repo) containing **Tasks**. Each Task runs one
of a small set of predefined **workflows**:

- **Type 1 — Chat**: freeform conversation/investigation with an agent.
- **Type 2 — Design doc** (deferred past v1): collaborative doc writing.
- **Type 3 — Coding task**: coder ↔ internal-reviewer loop, then a PR
  driven through automated checks and human review to approval.

The tool never talks to model APIs directly. It shells out to existing
agentic CLIs (`claude`, `codex`, `gemini`, ...) as subprocesses, through a
common adapter abstraction, so today it's built around Claude Code but
isn't structurally locked to it.

**v1 scope**: Projects/Tasks core + Type 1 end-to-end, then Type 3.
Type 2, notifications, auth, multi-git-host support, and concurrency caps
are explicitly out of scope for v1 (see §7 Deferred).

## 2. Architecture

```
                    ┌─────────────────────────────┐
   browser  ───────▶│   Web UI (React + TS)       │
  (SSH-tunneled)     │   static bundle, WS+HTTP    │
                    └──────────────┬──────────────┘
                                   │ HTTP + WebSocket (127.0.0.1 only)
                                   ▼
                    ┌─────────────────────────────┐
   choco             │      chokofactoryd          │
   (thin CLI) ──────▶│      (Rust backend)         │
   used by humans    │                              │
   & by agents       │  ┌────────────────────────┐ │
   inside tasks      │  │   HTTP/WS API layer     │ │
   (delegation)       │  └───────────┬────────────┘ │
                    │              ▼                │
                    │  ┌────────────────────────┐ │
                    │  │   Workflow Engine       │ │
                    │  │ (generic stage/transition│ │
                    │  │  interpreter, §5)        │ │
                    │  └───────────┬────────────┘ │
                    │       ┌──────┴───────┐        │
                    │       ▼              ▼        │
                    │ ┌───────────┐  ┌────────────┐│
                    │ │ Agent      │  │ Worktree /  ││
                    │ │ Runner     │  │ GitHub      ││
                    │ │ (adapters) │  │ integration ││
                    │ └─────┬─────┘  └────────────┘│
                    │       ▼                       │
                    │  subprocess: claude / codex /  │
                    │  gemini CLI                    │
                    │                                │
                    │  ┌────────────────────────┐   │
                    │  │  SQLite (state + events) │  │
                    │  └────────────────────────┘   │
                    └─────────────────────────────┘
```

Single Rust binary (`chokofactoryd`) hosting the API server, workflow
engine, agent runner, worktree manager, and GitHub integration, backed by
one SQLite file. A thin second binary (`choco`, the agent-facing CLI) is
just an HTTP client against the daemon — no logic of its own.
The React UI is a static bundle served by the same daemon.

### 2.1 Why one process

Simplicity given the requirements: single user, no auth, restart-safe via
SQLite. A single daemon avoids inter-process coordination for something
like the idle-subprocess reaper (§4.3) or the loop-guard logic (§5.3).

## 3. Core concepts / data model

SQLite tables (names indicative, not final schema):

- **`projects`**: `id, name, created_at`. Purely a grouping label —
  Projects hold no repo/path themselves.
- **`tasks`**: `id, project_id, workflow_def (name/path of the workflow
  definition driving this task — see §5), title, status, config (json),
  created_at, updated_at`. `config` holds per-task overrides (CLI/model/
  system-prompt per role, repo path, base branch, etc.) layered over
  workflow-definition-level and global defaults.
- **`task_runs`**: one row per underlying agent subprocess "session" a
  task has had (a task can span many runs across idle/resume cycles —
  every `agent_turn` stage execution is one run, keyed by which stage/role
  it belongs to). `id, task_id, stage, role, cli_adapter, model,
  session_id (from the CLI), status (active|idle|exited), started_at,
  ended_at`.
- **`events`**: append-only normalized event log. `id, task_run_id, seq,
  event_type (assistant_message|tool_call|tool_result|thinking|error|
  session_meta), payload (json), created_at`. This is what the UI
  timeline and the 1-year retention job operate on (§4.4).
- **`workflow_state`**: generic engine bookkeeping per task — current
  stage name, per-stage loop counters (for `loop_guard`, §5.3), and a
  stage-history trail. Shape is the same regardless of which workflow
  definition is driving the task; stage-specific data (e.g. PR URL,
  last check status) lives in each stage's own `payload` blob within this
  row rather than as dedicated columns, since new workflow definitions
  can introduce stage kinds with arbitrary data needs.

## 4. Agent adapter abstraction

A common trait every CLI adapter implements, roughly:

```rust
trait AgentAdapter {
    fn start(&self, prompt: &str, cfg: &RoleConfig) -> AgentHandle;
    fn resume(&self, session_id: &str, prompt: &str, cfg: &RoleConfig) -> AgentHandle;
    // AgentHandle streams normalized Events and accepts further messages
    // over stdin while the process is alive.
}
```

`ClaudeAdapter` wraps `claude --print --output-format=stream-json [--resume
<id>]`; `CodexAdapter`/`GeminiAdapter` follow the same shape against their
own CLIs. Each adapter is responsible for translating its CLI's native
stream format into the shared `Event` enum used in the `events` table, so
the Workflow Engine and UI never deal with CLI-specific formats.

### 4.1 Session lifecycle (Type 1 chat, and any live interaction)

Hybrid model per Q4:

1. **Active**: a task's conversation has a live subprocess. UI messages
   go over WS straight to the process's stdin; output streams back as
   Events, persisted and pushed to any connected UI.
2. **Idle**: after N minutes with no input (configurable, default TBD in
   plan), the daemon closes stdin, lets the process exit, and stores the
   CLI's `session_id` on the `task_runs` row (`status = idle`).
3. **Resume**: next message (from UI, or CLI/agent delegation) spawns a
   fresh process via `resume(session_id, ...)`, flips the run back to
   `active`.

This same mechanism underlies Type 3's coder/reviewer roles, just driven
by the Workflow Engine instead of direct user input.

### 4.2 Event normalization

Shared enum (illustrative):

```
Event::AssistantMessage { text }
Event::ToolCall { tool, input }
Event::ToolResult { tool, output, is_error }
Event::Thinking { text }
Event::SessionMeta { session_id }
Event::Error { message }
```

Full stream is stored per Q16 — nothing is summarized away at write time.
The UI decides how much detail to render (collapsed tool calls by
default, expandable).

### 4.3 Idle reaper

A background task in `chokofactoryd` periodically scans `task_runs` for
`active` runs past their idle threshold and tears them down (§4.1 step
2). Same mechanism handles daemon-restart recovery: any run left `active`
in the DB when the daemon starts back up is treated as dead (process is
gone) and flipped to `idle` using its last known `session_id`, so restart
just means "resume on next message," matching the SQLite-for-restart-
safety goal from the rough idea.

### 4.4 Retention job

Scheduled job (daily) deletes `events` rows older than 1 year (Q16).
Runs off `events.created_at`; doesn't touch `tasks`/`task_runs` rows
themselves, so task history/metadata outlives its detailed transcript.

## 5. Workflow Engine

Per your steer: rather than hardcoding Type 1 and Type 3 as bespoke Rust
state machines, the engine is a **generic interpreter over data-driven
workflow definitions** — a graph of named stages and the transitions
between them. Type 1 (chat) and Type 3 (coding task) become the two
built-in definitions shipped with the tool; you can copy/edit either one,
or author entirely new ones, without recompiling.

### 5.1 Workflow definition format

A workflow definition is a YAML file (e.g. `workflows/coding-task.yaml`)
describing stages and how they connect. Each stage has a **kind** (from a
fixed set implemented in Rust — see §5.2), the config that kind needs,
and an `on:` map from outcome → next stage name. Long prompts/system
prompts are referenced by file path rather than inlined, since the graph
file should stay readable:

```yaml
name: coding-task
roles:
  coder:
    cli: claude
    model: sonnet
    system_prompt_file: prompts/coder-system.md
  reviewer:
    cli: claude
    model: sonnet
    system_prompt_file: prompts/reviewer-system.md

stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: prompts/coder-turn.md      # templated with task input / prior feedback
    on: { done: internal_review }

  internal_review:
    kind: agent_turn
    role: reviewer
    prompt_file: prompts/reviewer-turn.md
    on:
      approved: open_pr
      changes_requested: coding
    loop_guard: { on: changes_requested, max: 3, then: escalate_to_human }

  escalate_to_human:
    kind: human_gate
    on: { resumed: coding }

  open_pr:
    kind: shell
    command: "gh pr create --fill --json url,number"
    capture: json                # parses stdout as JSON into this stage's payload
    on: { done: checks_polling, error: escalate_to_human }

  checks_polling:
    kind: poll
    command: "gh pr checks {{ stages.open_pr.number }} --json state -q '.[].state' | sort -u"
    interval: 30s
    timeout: 5m
    outcomes:
      - match: "^SUCCESS$"        -> green
      - match: "FAILURE|ERROR"     -> red
      - on_timeout                 -> timeout
    on:
      green: awaiting_human_review
      red: coding
      timeout: awaiting_human_review

  awaiting_human_review:
    kind: poll
    command: "gh pr view {{ stages.open_pr.number }} --json reviewDecision -q .reviewDecision"
    interval: 60s
    outcomes:
      - match: "APPROVED"            -> approved
      - match: "CHANGES_REQUESTED"   -> changes_requested
    on:
      approved: done
      changes_requested: coding

  done:
    kind: terminal
```

A workflow definition is referenced by name/path from a task's
`workflow_def` column (§3); the engine loads it once, resolves file
references (prompts, system prompts) relative to the definition file, and
drives the task's `workflow_state` (current stage, loop counters, per-
stage payload) through it.

**Templating/capture across stages**: a `shell`/`poll` stage's stdout can
be captured into that stage's `workflow_state` payload (`capture: json`
parses it as JSON, otherwise it's stored as raw text) and referenced by
later stages via `{{ stages.<name>.<field> }}` in their own `command:`
(and `agent_turn` stages can reference it in their `prompt_file`
template the same way — e.g. handing the reviewer the PR url). This is
the only templating the engine supports: variable substitution into
commands/prompts, not conditional logic — branching stays in `on:` maps
and `outcomes:` matching, not in the template language (§7).

### 5.2 Stage kinds (fixed set, implemented in Rust)

The graph's *topology* is fully data-driven, but each stage's *behavior*
comes from a small, fixed vocabulary of kinds — this is the deliberate
boundary that keeps the engine an interpreter rather than a general
scripting runtime (see §7 non-goal):

- **`agent_turn`**: runs one turn (or resumed session) of a role via the
  agent adapter abstraction (§4). Emits an outcome by inspecting the
  turn's result against kind-specific rules (e.g. a reviewer role's
  structured verdict maps to `approved`/`changes_requested`; a plain
  single-shot turn just emits `done`).
- **`shell`**: runs a one-shot command or `script_file` (same file-
  reference convention as prompts) to completion. Not git- or GitHub-
  specific — it's just "run this command" (`gh pr create`, a custom
  deploy script, anything). Exit code 0 → `done`, nonzero → `error`;
  `capture:` optionally parses stdout into the stage's payload for later
  stages to reference (see templating, §5.1).
- **`poll`**: runs a command repeatedly (`interval`) up to an optional
  `timeout`, matching its output against an `outcomes:` list (ordered
  substring/regex matches) to decide when/how to transition; `on_timeout`
  fires if `timeout` elapses with no match. Covers "wait on some external
  state" generically — GitHub check/review polling is just a `poll`
  stage with a `gh` command, not a dedicated GitHub kind.
- **`human_gate`**: pauses the task and waits for a human message (same
  live/async mechanism as chat, §4.1) before emitting `resumed`.
- **`terminal`**: marks the task finished; no `on:` transitions.

Worktree creation (§5.5) is the one repo operation the engine still
handles implicitly (on first entry into a stage that needs a working
copy) rather than as an explicit `shell` stage, since every task using a
coding-style workflow needs it and it's tied to task lifecycle/cleanup,
not a one-off command a workflow author would write per project.

New workflow definitions can only be built by composing these kinds. If a
genuinely new *kind* of behavior is needed later (not just a new graph
shape), that's a code change to the engine, not a config change — this
is intentional (§7).

### 5.3 Loop guards

`loop_guard` (Q13) is a per-stage, per-outcome counter: `max` transitions
through a given `on:` outcome before rerouting to `then:` instead. Reset
whenever the stage is entered from a *different* prior stage. This is how
"cap iterations, then escalate to human" is expressed generically rather
than as bespoke Type-3 logic.

### 5.4 Built-in workflow: Chat (Type 1)

Turns out this needs no new mechanics — it's the degenerate case of a
one-stage graph with no outgoing edges:

```yaml
name: chat
roles:
  chat:
    cli: claude
    model: sonnet
    system_prompt_file: prompts/chat-system.md   # optional

stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}        # no outcomes to transition on
```

Two things make this work without special-casing chat in the engine:

- **No `prompt_file`**: unlike `coding`/`internal_review`, this stage has
  no workflow-authored, templated prompt. The first message is whatever
  the human typed when creating the task; every message after that is
  live human input fed into the same session, not something the graph
  generates.
- **`on: {}`**: every other `agent_turn` stage runs to a conclusion and
  emits an outcome the engine looks up in `on:` to pick the next stage. A
  stage with nothing in `on:` has nowhere to go, so it never concludes —
  it just keeps accepting further messages into the same session
  indefinitely, which is exactly §4.1's active⇄idle⇄resume machinery.
  Chat isn't a distinct execution mode; it's what any `agent_turn` stage
  does by default while open, and chat is simply designed to stay open.

Investigation tasks are just chat tasks where the agent's own tool use
(reading logs, running commands via whatever tools its underlying CLI
exposes) does the work; ChokoFactory doesn't add its own skill system on
top (Q1 — we rely on the underlying CLI's own tools/skills).

One gap this surfaces: **closing/archiving a chat task isn't a stage
transition** — there's no outcome that leads anywhere, so `on:` can't
express it. Ending a chat task is a task-level operation (`tasks.status =
closed`, set directly via the API/UI/CLI) that sits outside the workflow
graph, not something the engine's stage/transition model covers.

### 5.5 Built-in workflow: Coding task (Type 3)

The YAML in §5.1 *is* the design for Type 3 — restated in prose:
coder produces a diff → internal reviewer approves or sends it back
(loop-guarded, escalates to a human gate on cap-out) → PR opened →
stage-1 poll of bots/lint/CI (config default ~5 min, red routes back to
`coding`) → stage-2 poll of human review (changes requested also routes
back to `coding`) → done. Matches Q5's two-stage default and Q13's
escalation behavior exactly, just expressed as data instead of Rust match
arms.

- **Role config resolution** (Q8): `coder`/`reviewer` resolve
  CLI/model/system-prompt from, in increasing specificity: global config
  → this workflow definition's `roles:` block → task-level `config`
  override.
- **Worktree manager** (Q7): triggered by the first stage that needs a
  working copy (`coding`); creates `git worktree add
  ../<project>-wt-<task-id> -b task/<task-id>` in the task's configured
  repo. Removed on reaching `done` (or task cancellation).
- **GitHub integration** (Q14, GitHub-only): not a dedicated Rust
  integration at all — `open_pr`, `checks_polling`, and
  `awaiting_human_review` are `shell`/`poll` stages whose commands happen
  to invoke `gh` (§5.1). The engine only needs to know how to run
  commands and match their output; GitHub-specific knowledge lives
  entirely in the workflow YAML and its `gh` invocations.
- **External reviewer config** (Q5): the specific poll `interval`/
  `timeout` and what counts as a "check" are just fields in the
  `coding-task.yaml` definition (or a task-level override) — a project
  can ship its own copy with entirely different stage-1 commands/
  matching, no Rust changes needed.

## 6. Interfaces

### 6.1 Web UI

- Project list → task list per project → task detail view.
- Task detail: live chat pane (WS) for Type 1 and for sending
  redirection messages to Type 3's `escalated_to_human` state; full
  event timeline (§4.2) for visibility into everything an agent did.
- New-task flow: pick project, workflow definition (chat, coding-task, or
  any custom one placed under `workflows/`), repo/working dir, role
  overrides (CLI/model/system prompt), initial prompt.
- No auth (Q15); backend binds `127.0.0.1` by default, accessed remotely
  via SSH port forwarding.

### 6.2 Agent-facing CLI (`choco`)

Both human-scriptable and agent-callable (Q12) — an HTTP client against
`chokofactoryd`'s API, e.g.:

- `choco task create --project <p> --workflow chat|coding_task --repo <path> --prompt <text> [--parent-task <id>]`
- `choco task status <id>`
- `choco task send <id> --text <text>`
- `choco task list [--project <p>] [--status <s>]`

`--parent-task` supports delegation: an agent running inside task A calls
this CLI to spawn task B, tagging B's `tasks.parent_task_id` so the UI
can show composition, and A can poll B's status the same way any external
script would.

## 7. Deferred / explicitly out of scope for this design

- **Type 2 (design doc workflow)** — storage/commenting model already
  decided (Q6: markdown file in repo + inline UI comments) but not
  implemented until a follow-up phase.
- **Notifications** (Q9).
- **Auth beyond localhost binding** (Q15).
- **Multi-git-host abstraction** (Q14) — GitHub only.
- **Global concurrency cap** (Q17) — unbounded for v1.
- **Cross-task shared project memory** (Q2 follow-up) — open question,
  not designed here.
- **General scripting/expression language for stage outcomes** — the
  workflow engine (§5) is intentionally an interpreter over a fixed set
  of stage kinds, not a Turing-complete workflow scripting system. Adding
  a genuinely new *kind* of stage behavior is a code change; only graph
  topology and stage parameters are data-driven. The `poll` kind's
  `outcomes:` matching (§5.2) is deliberately bounded to ordered
  substring/regex matches against command output plus an `on_timeout` —
  no boolean/arithmetic expression language, no access to arbitrary task
  state beyond `{{ stages.*.* }}` templating (§5.1).

## 8. Phasing

- **Phase 1**: Projects/Tasks core, SQLite schema, agent adapter
  abstraction (Claude Code adapter at minimum), session lifecycle
  (§4.1–4.3), event capture (§4.2, §4.4), Web UI (project/task
  list + live chat + timeline), agent-facing CLI. Workflow Engine built
  with just the stage kinds Type 1 needs (`agent_turn`, `human_gate`,
  `terminal`) plus the `chat.yaml` definition (§5.4) — validated via
  Type 1 end-to-end.
- **Phase 2**: Type 3 (coding task) — remaining stage kinds (`shell`,
  `poll`, including templating/capture across stages), loop guards
  (§5.3), the built-in `coding-task.yaml` definition (§5.5) and its `gh`-
  based commands, worktree manager.
- **Phase 3+ (not planned yet)**: Type 2 (design doc), notifications,
  anything else in §7.
