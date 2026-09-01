# 03 — Design

Based on `01-idea.md`. Research (Step 2) was skipped by explicit choice —
proceeding straight from idea to design.

## 1. Overview

ChocoFactory is a local tool for structuring how you work with coding
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
   choco             │      chocofactoryd          │
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

Single Rust binary (`chocofactoryd`) hosting the API server, workflow
engine, agent runner, worktree manager, and GitHub integration, backed by
one SQLite file. A thin second binary (`choco`, the agent-facing CLI) is
just an HTTP client against the daemon — no logic of its own.
The React UI is a static bundle served by the same daemon.

### 2.1 Why one process

Simplicity given the requirements: single user, no auth, restart-safe via
SQLite. A single daemon avoids inter-process coordination for something
like the idle-subprocess reaper (§4.3) or the loop-guard logic (§5.3).

### 2.2 On-disk layout

The tool is distributed as a binary (or built from a repo checkout, but
never run *out of* one) — nothing at runtime may assume a source
checkout is present or writable. So there has to be a separate,
user-owned location for everything the tool reads/writes that isn't the
binary itself, one that survives a binary upgrade/reinstall untouched:

- **`~/.config/chocofactory/`** is that root. Single directory, no
  XDG data/config split — this is a small single-user local tool, not
  worth the extra indirection.
- **`~/.config/chocofactory/config.yaml`**: global config (§5.5) — this
  user's default `cli`/`model`/`system_prompt_file` per role name,
  applied whenever a workflow-def doesn't pin that field itself. Absent
  file ⇒ no global defaults, not an error.
- **`~/.config/chocofactory/workflows/`**: every workflow definition the
  daemon can reference by name, `chat`/`coding-task` (the two built-ins)
  included. This is the one thing that needed the most thought: the
  built-ins ship compiled into the `chocofactoryd` binary (embedded at
  build time from the repo's own `workflows/` directory, which is the
  source of truth for their content) and are seeded out to this
  directory **on first run, only if not already present** — never
  overwritten on a later version's startup, so a user's edits to their
  own copy always survive an upgrade. §5 already intends this: *"you can
  copy/edit either one, or author entirely new ones, without
  recompiling"* — that only holds if what a user is copying/editing is a
  real file in a location the tool never touches again, not something
  hidden inside the binary.
- The SQLite file (§3) is a natural sibling here too
  (`~/.config/chocofactory/chocofactory.db`), though its exact wiring is
  the daemon-startup layer's call (§6.2/P1-9), not decided here.

Nothing above blocks a user (or a project) from pointing the daemon at a
different root via a flag/env var later if that turns out to matter —
just not needed yet with a single-user, single-machine deployment.

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
- **`events`**: append-only **task timeline** — every entry a task has
  accumulated, in one totally-ordered log. `id, task_id, task_run_id,
  event_type, payload (json), created_at`. This is what the UI timeline
  and the 1-year retention job operate on (§4.4).

  Most entries are normalized from an agent session and name the session
  they came from. Session attribution is *optional*, though: a
  `stage_entered` entry (§4.2) describes the task itself and leaves
  `task_run_id` NULL, because `human_gate` and `terminal` stages never
  open a session at all — there is no `task_runs` row to point at. Reading
  a task's timeline therefore filters `events.task_id` directly rather
  than joining through `task_runs`, which would silently drop exactly
  those entries.

  Ordering is always `(created_at, id)`, one rule for the whole table.
  There is no per-session sequence counter: events within a session are
  appended sequentially by a single drain loop with a full write between
  them, and `created_at` preserves whatever sub-second precision the
  platform clock offers (encoded to nanoseconds; ~1 µs granularity as
  measured on macOS), so a counter
  bought no ordering safety on the only path it covered. The trade-off
  taken deliberately is that clients cannot detect a missing entry by
  spotting a gap.
- **`workflow_state`**: generic engine bookkeeping per task — current
  stage name and per-stage loop counters (for `loop_guard`, §5.3). Shape
  is the same regardless of which workflow definition is driving the
  task; stage-specific data (e.g. PR URL, last check status) lives in each
  stage's own `payload` blob within this row rather than as dedicated
  columns, since new workflow definitions can introduce stage kinds with
  arbitrary data needs. The stage trail is *not* here — it is derived by
  filtering the task's timeline for `stage_entered` entries, which carry a
  timestamp and the outcome that caused each transition.

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
Event::HumanMessage { text }
Event::AssistantMessage { text }
Event::ToolCall { tool, input }
Event::ToolResult { tool, output, is_error }
Event::Thinking { text }
Event::SessionMeta { session_id }
Event::Error { message }
Event::StageEntered { stage, outcome }
```

Full stream is stored per Q16 — nothing is summarized away at write time.
The UI decides how much detail to render (collapsed tool calls by
default, expandable).

Two of these are not adapter output:

- `HumanMessage` is the human half of a conversation — the prompt a task
  was created with, or a message relayed into an already-open
  `agent_turn`. Without it the log recorded only the agent's replies.
- `StageEntered` is the workflow engine's own record of a stage
  transition, written at the single choke point every stage kind funnels
  through (§5.2), so one entry covers a task's entry stage, every
  subsequent transition, and terminal entry alike. `outcome` is the
  transition that selected the stage, null for the entry stage. Because
  it describes the task rather than a session it has no `task_run_id`
  (§3), and it is what makes a `human_gate`-only workflow observable at
  all — such a task opens no session and would otherwise emit nothing.

### 4.3 Idle reaper

A background task in `chocofactoryd` periodically scans `task_runs` for
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

One consequence of §3's move of the stage trail into `events`: the trail
is now subject to this job, where `workflow_state.stage_history` was
permanent. A task closed over a year ago keeps its `task_runs` metadata
but loses its `stage_entered` entries along with the rest of its
transcript. If the trail should outlive the transcript, retention needs
to exempt `event_type = 'stage_entered'` — deliberately not done here,
just noted so the choice is explicit rather than accidental.

### 4.5 ACP as a candidate adapter transport (under evaluation)

`ClaudeAdapter` (shipped, §8 P1-3) talks to `claude --print
--output-format=stream-json` directly and hand-parses its native event
stream. As of mid-2026, the **Agent Client Protocol** (ACP) — a JSON-RPC
2.0 standard originated by Zed and co-maintained with JetBrains, now with
a registry and adoption across Claude Code, Codex CLI, Gemini CLI, and
GitHub Copilot CLI — offers a standardized alternative to this
CLI-by-CLI parsing:

- `session/new` / `session/load` / `session/resume` map onto §4.1's
  active/idle/resume model as protocol-native operations rather than
  per-CLI flag conventions (`--resume <id>` today).
- `session/update` notification variants map close to 1:1 onto the
  existing `AgentEvent` enum (§4.2): `ContentChunk` → `AssistantMessage`,
  `ToolCallUpdate` → `ToolCall`/`ToolResult`, `ThinkingUpdate` →
  `Thinking`. Adopting ACP would change what produces these events, not
  the event shape itself or anything downstream (workflow engine, event
  store, UI stay untouched).
- `session/request_permission` gives a protocol-native tool-approval
  hook — not needed for anything in scope today, but relevant if a
  future stage kind wants mid-turn human approval.
- The practical win is on the abstraction goal stated in §2/§4 itself:
  one `AcpAdapter` implementation could replace N bespoke per-CLI stream
  parsers, since Claude Code, Codex CLI, and Gemini CLI all now have ACP
  support.

Open questions before committing to this as more than an experiment:
Claude Code's ACP support is a Node-based bridge package
(`claude-code-acp`, built on the Claude Agent SDK) rather than the
`claude` binary itself, adding a runtime dependency and a hop the direct
`stream-json` approach doesn't have; Codex CLI's ACP support is
described as a community bridge rather than first-party; and the
protocol is roughly a year old and still evolving. `AgentAdapter` (§4)
already isolates this behind a trait, so evaluating ACP is additive —
see §8 for the spike task — not a rewrite of anything shipped.

## 5. Workflow Engine

Per your steer: rather than hardcoding Type 1 and Type 3 as bespoke Rust
state machines, the engine is a **generic interpreter over data-driven
workflow definitions** — a graph of named stages and the transitions
between them. Type 1 (chat) and Type 3 (coding task) become the two
built-in definitions shipped with the tool; you can copy/edit either one,
or author entirely new ones, without recompiling.

### 5.1 Workflow definition format

A workflow definition is a YAML file (e.g.
`~/.config/chocofactory/workflows/coding-task.yaml`, §2.2) describing
stages and how they connect. Each stage has a **kind** (from a
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
    timeout: 5m                  # optional; killed and treated as `error` if exceeded
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

**Templating/capture across stages**: a `shell`/`poll` stage's stdout —
or an `agent_turn`'s reply (§5.2) — can be captured into that stage's
`workflow_state` payload (`capture: json`
parses it as JSON, `capture: text` stores it as raw text; a stage that
sets neither captures nothing) and referenced by
later stages via `{{ stages.<name>.<field> }}` in their own `command:`
(and `agent_turn` stages can reference it in their `prompt_file`
template the same way — e.g. handing the reviewer the PR url). This is
the only templating the engine supports: variable substitution into
commands/prompts, not conditional logic — branching stays in `on:` maps
and `outcomes:` matching, not in the template language (§7).

The captured value is stored at `payload.stages.<stage name>`, which is
what `{{ stages.<name>.<field> }}` resolves against. `stages` is an
explicit namespace inside the payload rather than the payload root, so
other engine-owned bookkeeping can join it later without a migration.
Re-entering a stage overwrites its previous capture — the value means
"what this stage produced most recently"; the `stage_entered` trail on
the events timeline is what records that it ran more than once.

Substitution rules, in full (P2-3):

- The path is `{{ stages.<name>[.<field>…] }}`, whitespace-tolerant
  around it but not inside it. Nested fields work (`stages.a.b.c`); a
  bare `{{ stages.<name> }}` is how a `capture: text` stage is read,
  since its payload entry is a plain string rather than an object.
- `stages` is the only namespace a template may read. Reserving the rest
  keeps room for engine-owned keys later without a definition that
  guessed at one silently changing meaning.
- Only scalars substitute: strings verbatim, numbers and booleans as
  their JSON text. `null`, objects and arrays are an error — capture is
  for short structured signals (a verdict, an id, a url), and splicing a
  blob into a shell command is not something the format promises.
- Only `command:` and `prompt_file` are templated. A `script_file` is an
  executable in its own right and its contents are left alone; so is
  live human input into an open `agent_turn`, which is what a person
  typed rather than something the graph composed.
- **Unresolvable references are errors, never empty strings.** Whatever
  the loader can settle it settles at load time: that the syntax parses,
  that the named stage exists, and that it declares a `capture:` at all
  — a reference to a stage that stores nothing can never resolve, and
  catching the typo when the definition is read beats discovering it as
  a parked task much later. What's left is genuinely run-time — a field
  the captured JSON turned out not to carry — and that fails the stage.
  Substituting nothing instead would hand `sh -c` a malformed command,
  or an agent a prompt with a hole in it, and the damage would surface
  far from its cause.
- Rendering happens once, when the stage is entered, against the payload
  as of that moment. A `poll` therefore runs the same rendered command
  on every attempt; nothing could change it mid-stage anyway, since
  captures are only written as part of a transition. The rendered
  command — not the template — is what the timeline records, because
  that is what actually ran.
- A run-time failure (a field the capture didn't carry, a value that
  isn't a scalar) fails the stage and is recorded on the task's timeline
  as an `error` entry naming the stage and the placeholder, not only in
  the daemon's log. It is the *expected* failure of this feature, since
  the loader cannot know a payload's shape, so it has to be visible
  where an operator looks.

**Two consequences worth stating plainly.**

*Substituted values are not shell-escaped.* A rendered `command:` runs
through `sh -c`, and what gets spliced in is now, under §5.2's
`agent_turn` capture, text an **agent** wrote — or text a `shell` stage
scraped from somewhere else (`gh pr view --json title`). A capture
containing `; rm -rf …` becomes part of the command the daemon runs.
This is accepted for now rather than overlooked: the roles a workflow
runs already have tool access to the same working copy, so this is not a
new capability so much as a new path to it. It is *not* equivalent
though — the daemon's shell sits outside whatever sandbox the agent's
own tools run in, and the content may be relayed from a third party (a
PR title) rather than authored by the role. Anyone templating a capture
into a command that does more than echo it should treat the value as
untrusted. Quoting on substitution was considered and rejected as a
silent behaviour change: `"{{ x }}"` in an already-quoted context would
gain literal quotes. If this needs closing, the honest fix is an
explicit filter syntax, which the format doesn't have yet.

*`{{` is reserved everywhere in an inline `command:`.* A command that
legitimately contains GitHub Actions syntax (`gh workflow run …
'${{ inputs.x }}'`) now fails to load, because the loader rejects any
placeholder whose root isn't `stages`. That's the cost of catching a
mistyped namespace at load rather than at run time. The escape hatch is
`script_file:`, whose contents are never templated.

### 5.2 Stage kinds (fixed set, implemented in Rust)

The graph's *topology* is fully data-driven, but each stage's *behavior*
comes from a small, fixed vocabulary of kinds — this is the deliberate
boundary that keeps the engine an interpreter rather than a general
scripting runtime (see §7 non-goal):

- **`agent_turn`**: runs one turn (or resumed session) of a role via the
  agent adapter abstraction (§4). A plain turn emits `done`. A turn that
  declares `capture:` (§5.1) keeps the agent's **final message** — read
  back off the timeline, since the engine holds no copy of a reply once
  the session drain has stored it.

  The final message specifically, not everything the agent said: a turn
  is not one message. An agent that uses a tool produces
  `assistant(text) → tool_call → tool_result → assistant(text)`, so
  capturing the lot would put its narration ("I'll read the diff
  first.") in front of its answer and a `capture: json` reply would
  never parse. What a verdict means is the last thing said, after the
  work. Text blocks within that message are concatenated with no
  separator, so a JSON document split across blocks survives reassembly.
  A reply that is *entirely* one ``` fence is unwrapped — the commonest
  thing a model does unbidden — and that is the only normalization
  applied; there is no search for JSON buried in prose, which would be
  guessing. Under `capture: json` the reply's
  reserved **`outcome`** key is what the stage transitions on, so a
  reviewer answering `{"outcome": "approved", "comments": "…"}` drives
  both the `on:` edge and the `comments` a later stage templates in —
  one mechanism rather than a verdict channel bolted alongside a capture
  channel. This is what §5.2 previously left as undefined "kind-specific
  rules".

  Capture is *only* taken when asked for. A stage with no `capture:`
  stores nothing, exactly as for `shell`/`poll`; without that rule a chat
  stage would rewrite its whole transcript into `workflow_state` on every
  turn. For the same reason `capture:` on an open-ended (`on: {}`) turn
  is rejected at load: such a stage never concludes, so there is no
  moment at which the capture could be taken, and accepting it would be
  accepting dead config.

  A reply that isn't valid JSON under `capture: json`, or that carries no
  usable `outcome`, is treated leniently — kept as text, outcome falls
  back to `done` — because that is the rule `shell`/`poll` already
  follow, and one rule for capture beats a stricter per-kind variant.
  The fallback is recorded as a `turn_outcome` entry on the timeline
  rather than only in the logs. The practical safety net is the `on:`
  map: a reviewer stage declaring `approved`/`changes_requested` and no
  `done` edge parks for a human instead of advancing, since there is no
  `done` transition to take. A stage that *does* declare `done` will take
  it, which is the accepted cost of the lenient rule.
- **`shell`**: runs a one-shot command or `script_file` (same file-
  reference convention as prompts) to completion. Not git- or GitHub-
  specific — it's just "run this command" (`gh pr create`, a custom
  deploy script, anything). Exit code 0 → `done`, nonzero → `error`;
  `capture:` optionally parses stdout into the stage's payload for later
  stages to reference (see templating, §5.1). An inline `command:` runs
  through `sh -c`, since these are shell strings and the examples above
  are pipelines; a `script_file:` is executed directly, so its `#!` line
  picks the interpreter (and it must be executable). Exit code is the
  *only* thing that decides the outcome — stdout that doesn't parse under
  `capture: json` is kept as text rather than failing the stage. An
  optional `timeout:` kills a command that runs too long and treats it as
  `error`; unlike an `agent_turn` there is no reaper behind a shell stage,
  so without one a hung command parks its task indefinitely. The command
  runs in the task's working directory, and what it did (exit code,
  duration, output tails) is recorded on the task's timeline as a
  `shell_output` event — a shell stage opens no agent session, so that
  entry belongs to the task and carries no `task_run_id`. A `timeout:`
  kills the command's whole process group, not just the shell the daemon
  spawned, so a timed-out pipeline can't leave grandchildren running in the
  working copy while the workflow retries. A process that escapes the group
  anyway (`setsid`, a double-fork) can still outlive the kill; that isn't
  silently tolerated — the stage reports it on the timeline so an operator
  knows a retry is about to run on top of something still live. Known gap: a shell stage
  interrupted by a daemon restart has no `task_run` row and no recovery
  hook, so its task parks at the stage it had entered — the same class of
  gap an interrupted `agent_turn` has, but without the stale-run sweep that
  covers that one.
- **`poll`**: runs a command repeatedly (`interval`) up to an optional
  `timeout`, matching its output against an `outcomes:` list (ordered
  substring/regex matches) to decide when/how to transition; `on_timeout`
  fires if `timeout` elapses with no match. Covers "wait on some external
  state" generically — GitHub check/review polling is just a `poll`
  stage with a `gh` command, not a dedicated GitHub kind. It takes the
  same `command:`/`script_file:` pair and `capture:` as `shell`, run the
  same way in the same working directory. Where it differs from `shell`
  is that **the exit code decides nothing**: a polled command failing is
  ordinary — `gh` on a rate limit or a dropped connection — and is
  precisely what polling exists to ride out, so only the output is
  matched and the loop keeps going. The one failure that ends it early is
  a command that could not be *started* at all (no `sh`, a `script_file`
  that isn't executable), which no amount of retrying fixes and which
  emits `error`; a stage that maps no `error` edge parks for a human
  instead. Patterns match against stdout only, in declaration order, with
  surrounding whitespace trimmed first — otherwise the `^SUCCESS$`
  example above could never match a command's newline-terminated output,
  and the stage would silently run to its timeout. `on_timeout` is spelled
  as a reserved `timeout` outcome in the ordinary `on:` map, which the
  loader requires whenever `timeout:` is set. Each attempt is capped at
  whatever is *left* of the budget rather than at `interval`, so a command
  slower than its own interval isn't killed on every attempt; the interval
  is then a delay measured after the command finishes, so attempts never
  overlap. What the command did is recorded as `shell_output` on the
  task's timeline, as for `shell` — but only for the attempt that resolves
  the stage and for attempts whose output *changed*, since a 30s poll over
  an hour is 120 identical `PENDING`s and the retention job prunes by age
  alone. A poll also re-checks that its task is still in the stage before
  each attempt, since it holds that window open long enough for a human to
  close or advance the task underneath it. Known gap, shared with `shell`
  and `agent_turn` but sharper here because a poll is *designed* to run
  for hours: a poll interrupted by a daemon restart leaves `current_stage`
  correct but no runner, and nothing re-enters the stage, so its task
  parks; no deadline is persisted either, so a future recovery sweep
  couldn't know how much of the budget was already spent. Tracked as
  [#52](https://github.com/itsypkin/ChocoFactory/issues/52).
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
exposes) does the work; ChocoFactory doesn't add its own skill system on
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
  (§2.2) → this workflow definition's `roles:` block → task-level
  `config` override. All three are keyed by role name with the same
  shape (`{cli, model, system_prompt(_file)}`) — including the
  task-level layer, e.g. `config: { roles: { reviewer: { model: opus } } }`
  — so overriding one role on one task never has to guess which role a
  flat field would've meant. `cwd` is the one task-`config` field that
  isn't per-role (it's task-wide), so it stays a flat top-level key
  alongside `roles`.
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
  any custom one placed under `~/.config/chocofactory/workflows/`, §2.2),
  repo/working dir, per-role overrides (CLI/model/system prompt),
  initial prompt.
- No auth (Q15); backend binds `127.0.0.1` by default, accessed remotely
  via SSH port forwarding.

### 6.2 Agent-facing CLI (`choco`)

Both human-scriptable and agent-callable (Q12) — an HTTP client against
`chocofactoryd`'s API, e.g.:

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
  not designed here. Tracked so it doesn't get silently dropped:
  [issue #40](https://github.com/itsypkin/ChocoFactory/issues/40).
- **General scripting/expression language for stage outcomes** — the
  workflow engine (§5) is intentionally an interpreter over a fixed set
  of stage kinds, not a Turing-complete workflow scripting system. Adding
  a genuinely new *kind* of stage behavior is a code change; only graph
  topology and stage parameters are data-driven. The `poll` kind's
  `outcomes:` matching (§5.2) is deliberately bounded to ordered
  substring/regex matches against command output plus an `on_timeout` —
  no boolean/arithmetic expression language, no access to arbitrary task
  state beyond `{{ stages.*.* }}` templating (§5.1).
- **Switching the adapter transport to ACP** (§4.5) — evaluated as a
  spike (§8), not committed. `ClaudeAdapter`'s direct `stream-json`
  parsing stays as-is unless the spike's findings justify the change.

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
- **ACP adapter spike** (§4.5): not gated on either phase — can run
  alongside Phase 2 since it only needs P1-3's `AgentAdapter` trait/
  `AgentEvent` shape to prototype against, and its outcome doesn't block
  anything else shipped or planned.
