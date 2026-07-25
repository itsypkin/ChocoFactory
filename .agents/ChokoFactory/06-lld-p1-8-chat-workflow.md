# 06 — Low-Level Design: P1-8 Built-in Chat Workflow

Issue: [#8](https://github.com/itsypkin/ChocoFactory/issues/8). Design ref:
`03-design.md` §5.4, §5.5 (Q8), §2.2 (on-disk layout — added alongside
this doc, see below). Depends on #7 (merged, PR #35).

This doc scopes the concrete change before writing any code, per the
project's spec-driven convention (`.agents/SOP/SpecDrivenDev.md`), and
because #7's review history (PRs #33/#34/#35) surfaced several
concurrency/eviction bugs late (`task_locks` eviction races, session
double-establish races, stale `end_reason` ambiguity). Section 4 exists
specifically to front-load that kind of analysis for the *new* surface
area this task adds, rather than finding it in review.

## 1. Scope recap

1. Ship `workflows/chat.yaml` (§5.4) — checked into the repo, embedded
   into the `chokofactoryd` binary, and seeded (write-if-missing) into
   the user's own `~/.config/chokofactory/workflows/` on first run (§2.6
   below, `03-design.md` §2.2). Nothing at runtime reads from the repo.
2. A workflow-name → file-path resolver over that seeded directory
   (doesn't exist yet — today `Task.workflow_def` is just a free string
   nothing reads).
3. A `create_task` operation: insert the task row, load its workflow
   definition, call the existing `WorkflowEngine::start_task` with the
   human's initial message.
4. A `send_message` operation: feed a follow-up human message into a
   task's already-open `agent_turn` stage.
5. Role config resolution (§5.5, Q8), scoped to one role: **global
   config → workflow-def `roles:` block → task-level `config`
   override**, all three keyed by role name (§2.3), reusable as-is when
   #17/P2-6 extends it to multiple roles.

Everything here is new library-level surface (`chokofactoryd` crate, no
binary wiring) — #9 (HTTP/WS API) is what will eventually call
`create_task`/`send_message` from a request handler. `main.rs` stays a
stub.

## 2. New artifacts

### 2.1 `workflows/chat.yaml`

Exactly §5.4's example, no `system_prompt_file` (optional, left unset by
default):

```yaml
name: chat
roles:
  chat:
    cli: claude
    model: sonnet

stages:
  chatting:
    kind: agent_turn
    role: chat
    on: {}
```

### 2.2 `chokofactoryd/src/global_config.rs` (new module)

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub roles: HashMap<String, GlobalRoleConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlobalRoleConfig {
    pub cli: Option<String>,
    pub model: Option<String>,
    pub system_prompt_file: Option<String>,   // resolved at load time, see below
}

pub enum GlobalConfigError { Io(std::io::Error), Yaml(serde_yaml::Error), InvalidFileReference { role: String, value: String } }

impl GlobalConfig {
    /// Missing file => `Ok(default())` (no global config is a normal,
    /// supported state, not an error) — only a *malformed* file is an error.
    pub fn load(path: &Path) -> Result<Self, GlobalConfigError> { ... }

    /// `$HOME/.config/chokofactory/config.yaml`, or `None` if `$HOME`
    /// isn't set (caller then skips loading and uses `default()`).
    pub fn default_path() -> Option<PathBuf> { ... }
}
```

Shape mirrors the workflow-def `roles:` block deliberately (same fields,
keyed by role name) so #17 can extend both in lockstep. `system_prompt_file`
here resolves relative to the **global config file's own parent
directory**, reusing the same traversal guard as the workflow loader
(§2.4) — same rationale as `workflow_def.rs`'s existing `resolve_file`:
this is an operator-controlled file, not request input, but there's no
reason to hold it to a lower bar.

**No caching** — see §4.5. `GlobalConfig::load` is called fresh every time
a role needs resolving; `WorkflowEngine` stores the *path*, never a parsed
`GlobalConfig`.

### 2.3 `chokofactoryd/src/role_config.rs` (new module)

```rust
pub struct ResolvedRoleConfig {
    pub cli: String,
    pub model: String,
    pub role_config: RoleConfig,   // adapter::RoleConfig — cwd/model/system_prompt, unchanged shape
}

pub enum RoleConfigError {
    MissingField { role: String, field: &'static str },  // "cli" or "model" unresolved after all 3 layers
}

pub fn resolve(
    role_name: &str,
    role_def: &RoleDef,        // workflow-def layer (now partial, §2.4)
    global: &GlobalConfig,     // global layer
    task_config: &Value,       // task-level layer — { cwd, roles: { <name>: {...} } }, see below
    cwd: PathBuf,
) -> Result<ResolvedRoleConfig, RoleConfigError>
```

Per-field precedence (task-level wins, then workflow-def, then global,
independently per field — not "whole layer wins"). **All three layers
are keyed by role name with the same shape** — task-level is *not* a
flat bag of fields, it's `{ roles: { <role_name>: { cli, model,
system_prompt } } }`, structurally identical to `GlobalConfig`'s
`roles:` map and the workflow-def's `roles:` block:

| Field | Task-level (`task.config.roles.<name>.*`) | Workflow-def (`roles.<name>.*`) | Global (`roles.<name>.*` in `config.yaml`) |
|---|---|---|---|
| `cli` | `String` | `Option<String>` | `Option<String>` |
| `model` | `String` | `Option<String>` | `Option<String>` |
| system prompt | `system_prompt: String` (inline text) | `system_prompt_file` (already-resolved `PathBuf`, read now) | `system_prompt_file` (resolved `PathBuf`, read now) |

`task_config.get("roles").and_then(|r| r.get(role_name))` is looked up
once per `resolve()` call, then each field read off that same
sub-object — mechanically identical to how the global and workflow-def
layers are read, just a different `Value`/struct as the source. `cwd` is
the one field that stays flat at the top of `task_config`
(`task_config.get("cwd")`, unchanged from today) — it's task-wide, not
per-role, so it doesn't belong under `roles.<name>`.

Why keyed instead of flat: a flat `config.model` is only unambiguous
because `chat.yaml` happens to have exactly one role. The moment a
workflow has more than one (`coder`/`reviewer` in `coding-task.yaml`,
#17/P2-6), "override the model for this task" is inherently a per-role
question — and the design doc's own new-task-flow description already
calls these "per-role overrides," not one flat knob (§6.1). Keying by
role now costs one extra level of JSON nesting and means #17 needs no
schema migration later.

If `cli` or `model` is still unresolved after all three layers →
`RoleConfigError::MissingField`. System prompt has no such requirement —
`None` all the way down just means no system prompt, matching current
behavior.

Field reads within a role's task-level object follow the existing
precedent (`task.config.get("cwd").and_then(Value::as_str)` in
`engine.rs` today): wrong JSON type or absent key ⇒ "not overridden,"
never an error. Same for the `roles`/`<name>` lookups themselves — a
missing or wrong-shaped `roles` key just means "no task-level overrides
for anyone," not a validation error. This is a deliberate, existing
convention, not new leniency introduced here.

**Deliberate omission**: no file-path field anywhere under
`task_config.roles.<name>` (no `system_prompt_file` override there, only
inline `system_prompt` text). `task.config` is arbitrary JSON that will
eventually arrive over HTTP (#9) — the least-trusted of the three
layers — so it's the one layer that can't ask the daemon to read an
arbitrary path off disk. This sidesteps reopening the exact class of
path-traversal bug the loader had to close in review (`f9219ab`), from a
source with a materially weaker trust boundary than a checked-in
workflow YAML file.

`engine.rs`'s `enter_agent_turn` is refactored to call `role_config::resolve`
instead of reading `role_def.cli`/`role_def.model` directly — one code
path computes the final config, used by both the existing agent_turn
entry and the new `send_message`.

### 2.4 Modifies `chokofactoryd/src/workflow_def.rs`

`RoleDef.cli`/`RoleDef.model` change from `String` to `Option<String>`
(and `RawRole`'s matching fields gain `#[serde(default)]`). **This is a
change to already-merged code from #6/#7**, flagged explicitly because a
reviewer familiar with that history should see it coming, not discover it
mid-diff:

Without this, a workflow-def's `roles:` block is *required* to fully
specify `cli`+`model` for every role it declares (today's schema), which
makes the global-config layer permanently dead for those two fields —
there'd be no way for a role to ever fall through to it. Q8's 3-layer
resolution only means something if the middle layer is allowed to be
partial.

`WorkflowDefinition::validate()` doesn't need a new check: "must resolve
to *something*" is no longer decidable at parse time (it depends on
`GlobalConfig`, which isn't loaded yet when a workflow file is parsed) —
that's exactly what `RoleConfigError::MissingField` at resolution time is
for instead.

Existing tests in this file that construct `RoleDef { cli: "x".into(), ... }`
need mechanical updates to `Some("x".into())`.

**Also extracted from this file**: `resolve_file`'s traversal guard (reject
absolute paths / `..` components, then check the joined path exists) moves
to a small shared helper — e.g. `chokofactoryd/src/fileref.rs::resolve_relative`
— with `workflow_def.rs::resolve_file` becoming a thin wrapper around it
that keeps its own error type/messages. `global_config.rs` (§2.2) calls
the same shared helper for `system_prompt_file`, rather than
reimplementing the same three lines of path-safety logic a second time.

### 2.5 New DB helper: `chokofactoryd/src/db/task_runs.rs`

```rust
/// Most recent task_run recorded against `task_id` for `stage` — the
/// run currently "open" for that stage, if any.
pub async fn get_current_for_stage(
    pool: &SqlitePool, task_id: &str, stage: &str,
) -> Result<Option<TaskRun>, sqlx::Error> {
    // SELECT ... WHERE task_id = ? AND stage = ? ORDER BY created_at DESC LIMIT 1
}
```

### 2.6 Config root, defaults, and seeding built-in workflows (new module, `chokofactoryd/src/config_root.rs`)

Per `03-design.md` §2.2: the tool is distributed as a binary and must
never assume a source checkout is present at runtime, so built-in
workflows and user overrides can't live anywhere near wherever the
binary itself sits. Everything user-owned lives under one root:

```rust
/// `$HOME/.config/chokofactory`, or `None` if `$HOME` isn't set (callers
/// then fall back to whatever explicit path they were given/default to
/// for tests — never fail startup just because $HOME is unset).
pub fn config_root() -> Option<PathBuf> { ... }
```

Both `GlobalConfig::default_path()` (§2.2 of this doc) and
`WorkflowEngine`'s default `workflows_dir` build on this one function —
`config_root().map(|r| r.join("config.yaml"))` and
`config_root().map(|r| r.join("workflows"))` respectively — so there's a
single place that decides "where does chokofactory's home live," not two
copies of the `$HOME/.config/chokofactory` prefix that could drift apart.

```rust
/// The two workflows compiled into the binary — checked into the repo's
/// own `workflows/` directory as the source of truth, embedded at build
/// time. (Just `chat` for #8; `coding-task` joins this list in #18/P2-7.)
const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[
    ("chat", include_str!("../../workflows/chat.yaml")),
];

/// Writes each of `BUILTIN_WORKFLOWS` into `workflows_dir/<name>.yaml`,
/// but only if that file doesn't already exist — an existing file (a
/// user's edited copy, or a stale version from a previous release) is
/// never overwritten. Creates `workflows_dir` itself if missing.
///
/// Not folded into `WorkflowEngine::new` — matches the project's
/// existing convention of keeping constructors side-effect-free and
/// having the daemon's startup sequence explicitly call the I/O-bearing
/// setup steps (`session.rs`'s idle reaper and
/// `task_runs::recover_stale_active_runs` are both already called this
/// way, not from inside a constructor).
pub fn seed_builtin_workflows(workflows_dir: &Path) -> std::io::Result<()> { ... }
```

`#9` (or `#8`'s own integration tests, since there's no daemon startup
yet) is responsible for calling `seed_builtin_workflows` once before
first use; nothing in `create_task`/`send_message` calls it implicitly —
consistent with §4.5's "no hidden state" stance elsewhere in this doc.

### 2.7 New `WorkflowEngine` methods (`engine.rs`)

Two new fields on `WorkflowEngine`: `workflows_dir: PathBuf` (the seeded
user directory from §2.6, *not* a repo-relative path), `global_config_path:
Option<PathBuf>` (both threaded through `new(...)`, which changes
signature — updates all existing call sites/tests).

```rust
pub async fn create_task(
    self: &Arc<Self>,
    project_id: &str,
    workflow_def_name: &str,   // e.g. "chat" — untrusted, will come from #9's request body
    title: &str,
    initial_input: &str,
    config: Value,
) -> Result<Task, CreateTaskError> {
    // 1. resolve_workflow_path(&self.workflows_dir, workflow_def_name)?  (§2.8 — strict allowlist, not the loader's path-ref rules)
    // 2. WorkflowDefinition::load(path)? wrapped in Arc::new(..)   — freshly parsed, not cached (§4.5)
    // 3. tasks::create(...)  — fresh UUID, no create-time collision possible
    // 4. self.start_task(&task.id, &definition, Some(initial_input)).await?
}

pub async fn send_message(
    self: &Arc<Self>,
    task_id: &str,
    text: &str,
) -> Result<(), SendMessageError> {
    // 1. workflow_state::get(pool, task_id)? -> current_stage
    // 2. re-resolve + re-load this task's WorkflowDefinition (same path as create_task step 1-2 — no cache, see §4.5)
    // 3. look up current_stage in definition.stages; require StageKind::AgentTurn { .. } with an
    //    EMPTY `on` map — anything else is rejected with SendMessageError::StageNotOpenEnded
    //    (this is the guard discussed in §4.3 — not a Phase-1 gap, a hard boundary)
    // 4. task_runs::get_current_for_stage(pool, task_id, current_stage)? -> task_run, else NoOpenRun
    // 5. role_config::resolve(...) for that stage's role (same helper as enter_agent_turn)
    // 6. session_manager.send_message(&task_run.id, text, &resolved.role_config).await
    //    (SessionError propagated through SendMessageError — no new handling needed, §4.4)
}
```

Note `create_task` and `send_message` don't need `task_id`/`workflow_def`
to be looked up from a shared in-memory registry — everything they need
(`tasks` row, `workflow_state` row, `task_runs` row) is read fresh from
SQLite each call, same as every other engine method today.

### 2.8 Workflow-name resolver

Lives with the new `WorkflowEngine` methods (or a small free function in
`workflow_def.rs` — either is fine, no strong reason to pick one over the
other):

```rust
fn resolve_workflow_path(workflows_dir: &Path, name: &str) -> Result<PathBuf, ResolveError> {
    // allowlist, not the loader's blocklist (§4.2): name must be non-empty
    // and match ^[A-Za-z0-9_-]+$ — no `/`, no `.`, no `..` possible by construction.
    // then workflows_dir.join(format!("{name}.yaml")), check .is_file()
}
```

`workflows_dir` here is always the seeded `~/.config/chokofactory/workflows/`
(§2.6), whether `name` resolves to a built-in (`chat`) or something a
user authored/copied there themselves — by the time this function runs,
both are just files in the same directory, indistinguishable to the
resolver.

## 3. Data flow

```
seed_builtin_workflows(workflows_dir)        (once, at daemon startup — §2.6)
  -> ~/.config/chokofactory/workflows/chat.yaml written, if not already present

create_task("chat", "hey, look into the flaky test", config: json!({}))
  -> resolve_workflow_path("chat")           -> ~/.config/chokofactory/workflows/chat.yaml
  -> WorkflowDefinition::load(...)           -> Arc<WorkflowDefinition>
  -> tasks::create(...)                      -> Task { id, workflow_def: "chat", ... }
  -> engine.start_task(id, def, Some(msg))   -> workflow_state row at "chatting"
                                              -> enter_agent_turn -> role_config::resolve
                                              -> session_manager.start(...) -> live subprocess

create_task("chat", "...", config: json!({ "roles": { "chat": { "model": "haiku" } } }))
  -> same as above, but role_config::resolve's task-level lookup for
     role "chat" finds { model: "haiku" } and it wins over chat.yaml's
     `model: sonnet` and any global-config default for role "chat"

send_message(task_id, "actually check the other branch too")
  -> workflow_state::get(task_id)            -> current_stage = "chatting"
  -> (reload def) stages["chatting"]         -> AgentTurn, on: {} -> OK, open-ended
  -> task_runs::get_current_for_stage(...)   -> the task_run from start_task above
  -> role_config::resolve(...)               -> same resolved config
  -> session_manager.send_message(...)       -> forwards to live process, or resumes if idle
```

## 4. Concurrency & consistency analysis

Given the review history on `task_locks` (races on `advance()`/
`start_task()`, eviction racing a concurrent holder, `Arc::strong_count`
bookkeeping), this section exists to make explicit which parts of the new
surface need similar care and which don't — and why.

### 4.1 `create_task` needs no new locking

Task IDs are server-generated (`Uuid::new_v4()` inside `tasks::create`) —
there's no client-supplied identifier two concurrent callers could
collide on, so there's no "two creates racing for the same row" case to
guard against. `start_task` (called internally) already takes its own
per-task lock (existing, unchanged). By the time `create_task` returns
`Ok`, `start_task` has fully run — the task's `workflow_state` row and
first `task_run` row are committed, or the whole call returned `Err` and
neither exists in a way a caller could act on (a caller can't call
`send_message` before it has a `task_id`, and it doesn't have one until
`create_task` returns). This closes, by construction, the one race that
would otherwise matter: "a follow-up message arrives before the task
finished being created."

### 4.2 The workflow-name resolver needs an allowlist, not the loader's blocklist

`workflow_def.rs::resolve_file` (used for `prompt_file`/
`system_prompt_file`/`script_file`) rejects absolute paths and `..`
components — appropriate for a *relative path* written by whoever authored
a workflow YAML file already sitting on disk. `workflow_def_name` here is
different: a single opaque identifier that will eventually come straight
from an HTTP request body (#9) or CLI arg (#10) — materially less
trusted. Rather than reusing the blocklist and hoping it covers every
character combination `PathBuf::join` might do something surprising with,
§2.8 uses a strict allowlist (`^[A-Za-z0-9_-]+$`) so there's no path
syntax left to reason about at all.

### 4.3 `send_message`'s stage lookup: the race that *would* exist in a naive generic version, closed by scoping

A generic "send a message to whatever stage this task is currently in"
function has a real TOCTOU race for a workflow where stages *do*
transition: read `current_stage` = A, look up A's task_run, but before
the message is delivered `advance()` (triggered by something else — e.g.
a `human_gate` resume, once #9 wires that) moves the task to stage B and
starts a new task_run for B. The message then goes to a task_run that's
no longer "current," or to a stage about to be superseded.

This is closed here, not worked around: `send_message` requires the
looked-up stage to be `AgentTurn` with an **empty `on:` map**. Empty `on:`
is a static, load-time-checked property (the loader already guarantees
every `on:` target names a real stage — an empty map is simply "zero
transitions declared") — a stage in this shape can never be advanced,
full stop (`advance()` looks up `outcome` in `on:`, and there is nothing
to look up; any outcome fails with `UnknownOutcome`). So for `chat.yaml`
specifically, "read current_stage, then look up its task_run" isn't
merely unlikely to race a concurrent transition — no code path exists
that could ever move this task off `"chatting"`. The check makes that
invariant explicit and machine-enforced rather than "true today because
of how chat.yaml happens to be written." Any future workflow whose entry
stage does transition gets a clear `SendMessageError::StageNotOpenEnded`
instead of a silent, latent race — the generalization to relaying
messages into stages that *can* transition (i.e. `human_gate`) is
explicitly #9's problem, matching `engine.rs`'s own existing note that
human_gate's `resumed` relay "[is] not yet wired to an API layer (P1-9)."

### 4.4 No new lock in front of `session_manager.send_message`

Two `send_message` calls landing concurrently for the same task both
resolve to the same `task_run_id` (per §4.3, that's stable) and both call
into `SessionManager`, which already serializes on it: if live, both sends
go through an unbounded `mpsc` channel (safe for concurrent producers,
processed one at a time by `drain_session`'s single select loop —
ordering between two nearly-simultaneous human messages isn't
guaranteed, which is inherent to "two messages sent close together," not
a bug); if idle, `reserve()`'s atomic claim means only one caller resumes
and the other gets `AlreadyStarting` (existing, already-reviewed
behavior from PR #28/#35). Adding a second lock in `send_message` ahead of
this would duplicate a guarantee `SessionManager` already provides at the
correct granularity (the `task_run_id`, the actual contended resource) —
and would itself be new surface for exactly the kind of stale-eviction
bug `task_locks` had. Deliberately not adding it.

### 4.5 No caching, anywhere, in this change — the eviction bug class doesn't apply

Every past review finding in this area (`task_locks` eviction racing a
concurrent holder, the `Arc::strong_count` bookkeeping in `engine.rs`)
came from a cache whose entries needed to be invalidated/removed at the
right time under concurrency. This change introduces zero new caches:

- `GlobalConfig` is re-read and re-parsed from disk on every
  `role_config::resolve` call. `WorkflowEngine` stores a `PathBuf`, never
  a parsed `GlobalConfig`.
- `WorkflowDefinition` is re-loaded and re-parsed from disk on every
  `create_task`/`send_message` call, exactly matching how `start_task`/
  `advance` already expect a freshly-supplied `Arc<WorkflowDefinition>`
  from their caller today (nothing in the existing engine caches these
  either).

Cost: a few KB of YAML re-read and re-parsed per human message, against
an LLM turn that takes seconds — negligible, and it buys freedom from an
entire bug class the reviewer has twice now had to chase down in this
codebase. If this ever shows up as a real bottleneck, adding a cache is a
follow-up with its own invalidation design (likely keyed by mtime), not
something to bolt on speculatively here.

### 4.6 `task_runs::get_current_for_stage` ordering

`ORDER BY started_at DESC LIMIT 1` needs `started_at` to actually
disambiguate concurrent inserts (`task_runs` has no `created_at` column —
`started_at` is the timestamp that exists). `chat.yaml`'s stage only ever
gets one `task_run` ever created for it (no loop, no re-entry), so this
query returning "the" row rather than "the most recent of several" is the
common case by construction, not something the ordering has to work hard
for. Note this is deliberately *not* the same ordering as the existing
`task_runs::list_for_task` (which orders `BY id`, a random UUID with no
temporal meaning) — `get_current_for_stage` needs a real time-ordering
that `list_for_task` never needed, not a continuation of its convention.
The ordering here is correct in general, so this helper is safe to reuse
as-is once other workflows with re-entrant stages exist.

### 4.7 `seed_builtin_workflows` needed an atomic create, not `exists()` + `write()`

Caught in review, not in the original draft of this doc: the first
implementation checked `!path.exists()` before writing — a check-then-act
race. Two daemon processes seeding the same `workflows_dir` at once
(started concurrently, or overlapping during a supervisor restart) could
both observe "missing" before either writes, which doesn't corrupt
anything here (both would write the same embedded bytes) but does
contradict the "never overwritten" guarantee this function exists to
provide, and is exactly the kind of check-then-act gap worth closing on
principle given this repo's history with exactly this bug class
elsewhere. Fixed by using `OpenOptions::new().write(true).create_new(true)`
(atomic create-or-fail) and treating `ErrorKind::AlreadyExists` as
success — the file being present is the desired end state regardless of
which caller's write actually landed.

### 4.8 `GlobalConfig`'s `roles:` block needed the loader's duplicate-key guard too

Also caught in review: `workflow_def.rs`'s `roles:`/`stages:` maps reject
a repeated YAML key rather than silently keeping only the last entry
(`deserialize_map_rejecting_duplicate_keys`, itself added in an earlier
review round on #33/#35) — but `global_config.rs`'s own `roles:` block,
added by this PR, used a plain `HashMap` deserialization and didn't pick
up the same guard, reopening exactly the bug this repo already fixed
once elsewhere. A hand-edited `~/.config/chokofactory/config.yaml` with a
copy-pasted `roles:` entry would parse successfully and silently drop the
first one — a real, silent-data-loss correctness bug on legitimate user
input, not just a hypothetical.

Fixed by extracting the guard out of `workflow_def.rs` into a new shared
`serde_util.rs` (`deserialize_map_rejecting_duplicate_keys`, unchanged
logic) and having both `workflow_def.rs` and `global_config.rs` use it —
one implementation of "reject a duplicate map key," not two copies that
could drift, and not a second place for the exact same authoring-typo
class to slip back in.

## 5. Error handling

New error enums (`CreateTaskError`, `SendMessageError`,
`RoleConfigError`, workflow-name `ResolveError`, `GlobalConfigError`)
follow the existing project convention (`EngineError`,
`WorkflowDefError`, `SessionError`): plain enums, `Display` impls with
lower-case, no-punctuation messages, `From<sqlx::Error>`/`From<std::io::Error>`
where a variant just wraps one. No `anyhow`/`thiserror` — matches what's
already in the crate.

`SendMessageError` wraps `EngineError`-style variants
(`NoWorkflowState`, `UnknownStage`) for the lookup steps, adds
`StageNotOpenEnded`, `NoOpenRun`, and wraps `SessionError` for the final
delivery step — nothing there needs new handling, `SessionError`'s
existing variants (`NotResumable`, `AlreadyStarting`, `Adapter`, `Db`)
already cover every way delivery can fail.

## 6. Testing plan

- `role_config::resolve` unit tests: all 2^3-ish precedence combinations
  per field (task-level / workflow-def / global / absent-everywhere →
  `MissingField`); confirms task-level `system_prompt` (inline) beats
  workflow-def `system_prompt_file` beats global `system_prompt_file`.
- **Genericity over role name** (directly de-risks #17, which now
  asserts this needs no new resolution logic): build one `GlobalConfig`
  and one task-level `config` `Value` that both carry overrides for two
  *different*, made-up role names (e.g. `"coder"` and `"reviewer"`, or
  arbitrary names unrelated to `chat`) at once, then call `resolve()`
  once per name and assert each call only ever sees its own role's
  entry — the `"coder"` override never leaks into the `"reviewer"`
  result or vice versa, and neither is affected by `chat.yaml` not
  having either role. This is the test that would fail if `resolve`'s
  `task_config`/`GlobalConfig` lookups ever special-cased `"chat"`
  instead of trusting the `role_name` parameter — the exact mistake §4
  warns against, not something the type signature alone rules out.
- `resolve_workflow_path` unit tests: rejects `..`, `/`, empty string,
  non-matching characters; accepts a plain name; missing file → distinct
  error from invalid name.
- `GlobalConfig::load`: missing file → `default()`, malformed YAML →
  error, partial file (only some roles/fields) → the rest genuinely
  absent (not defaulted to empty string).
- Loader tests updated for `RoleDef.cli`/`model` → `Option<String>`
  (mechanical).
- Integration test (same `fake_claude.py`/`fake_claude_oneshot.py`
  fixture pattern already used in `session.rs`/`engine.rs` tests):
  `create_task("chat", ...)` starts a session and the initial message
  reaches the fake adapter; a follow-up `send_message` reaches the same
  live process; a follow-up after the fake process exits resumes it
  (exercises the existing resume path, now reached through the new
  wrapper).
- `send_message` against a hand-built definition whose entry stage has a
  non-empty `on:` → asserts `StageNotOpenEnded`, not a silent send.
- `seed_builtin_workflows`: writes `chat.yaml` into an empty temp dir;
  run twice with a modified `chat.yaml` in between (simulating a user
  edit) → second run leaves the edit untouched (write-if-missing, never
  overwrite); creates the directory itself if absent.
- `config_root`/`GlobalConfig::default_path`/the default `workflows_dir`
  path derivation: exercised against an explicit path parameter rather
  than mutating `$HOME` in-process (test isolation — env var mutation
  isn't safe across parallel test threads); the `$HOME`-reading function
  itself stays a thin, untested-in-isolation wrapper, consistent with how
  `GlobalConfig::load`/`WorkflowDefinition::load` already separate
  "resolve a path" from "read from an explicit path" for testability.

## 7. Explicit non-goals (deferred)

- Multi-role resolution (more than one role per task) — #17/P2-6.
- Relaying a human message into a `human_gate`'s `resumed` outcome, or
  any stage that can transition — #9, once there's an API layer to
  receive it.
- Dynamic adapter dispatch by `cli` string (today there's exactly one
  `Arc<dyn AgentAdapter>` wired into `SessionManager` at construction;
  the resolved `cli` value is still just recorded on `task_runs` for
  bookkeeping, as it already is today) — not asked for by this issue.
- Wiring any of this into `main.rs` — no daemon startup config loading
  exists yet; that's #9's job.
