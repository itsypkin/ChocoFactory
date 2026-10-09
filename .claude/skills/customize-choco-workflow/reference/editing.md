# Editing a workflow: rules and traps

Paths in a workflow are relative to the YAML file. Edit a copy of a
built-in rather than starting from nothing. The full rules are in
[docs/workflows.md](https://github.com/itsypkin/ChocoFactory/blob/main/docs/workflows.md).

## Contents
- Stage kinds and their keys
- Routing on a verdict
- Loop guards
- Poll stages and watchers
- Human gate markers
- Prompts and templates
- Shell stages
- Roles
- Adding a check: an example

## Stage kinds and their keys

Every stage takes `kind`, `on` and `loop_guard`. The first stage listed is
where a task starts.

| Kind | Other keys |
|---|---|
| `agent_turn` | `role`, `prompt_file`, `capture`, `report_sections` |
| `shell` | `command` or `script_file` (exactly one), `capture`, `timeout`, `env` |
| `poll` | `command` or `script_file`, `capture`, `env`, `interval` (required), `backoff`, `timeout`, `outcomes` |
| `human_gate` | `capture` (`text` only), `markers`, `watch` |
| `terminal` | none, and no `on:` |

A misspelt stage key or role field is rejected when the workflow loads. A
misspelt top-level key is not: only `name`, `roles`, `stages` and
`worktree` are read, and anything else is silently ignored. A typo such as
`worktee: true` leaves `worktree` false, so agents run in the repo checkout
instead of a disposable worktree. A workflow with a `read_only` role
catches that, because it needs `worktree: true`; one without doesn't.

## Routing on a verdict

With `capture: json`, the keys of `on:` are the verdicts the agent may
report. Without it the stage can only report `done`, so its `on:` needs a
`done` key. Load doesn't check that: an `agent_turn` without `capture: json`
and without `done` loads, and its turn can never advance. An `agent_turn`
with `on: {}` is a standing chat-style session and takes no `capture` or
`report_sections`.

`report_sections` is a list of headings the agent's report must contain;
names are non-empty and distinct. See
[routing on an agent's verdict](https://github.com/itsypkin/ChocoFactory/blob/main/docs/workflows.md#routing-on-an-agents-verdict).

## Loop guards

`loop_guard: { on, max, then }` counts how many times in a row the stage
left through `on`. The (max+1)th time, the task goes to `then`. Any other
outcome resets the count, and so does arriving at `then`. Load rejects:

- an `on` that isn't a key of the stage's `on:`;
- a `then` that isn't a stage;
- a `then` that sits on every path back to the guarded stage, because it
  would reset every lap and never trip.

## Poll stages and watchers

A `poll` stage and a `human_gate` `watch:` run a command or script every
`interval` and match its output against `outcomes` (each a `match` regex and
a `then`).

- `interval` is required. `timeout` is one budget from stage entry, and a
  `timeout` needs a `timeout` key under the stage's `on:`.
- `backoff` steps are `{ after, interval }`. The list is not empty, `after`
  strictly increases, and each is before `timeout`.
- Durations use `s`, `m` or `h` only, and are not zero.
- Every `outcomes[].then` must be a key of `on:`.

See [a human gate that watches for its answer](https://github.com/itsypkin/ChocoFactory/blob/main/docs/workflows.md#a-human-gate-that-watches-for-its-answer).

## Human gate markers

`markers:` are lines a person writes in a reply, each with a `then`. The
whole line is matched, case-sensitive. A reply with no marker line is
refused and nothing is sent. Each `then` must be a key of `on:`.
Lines can't be empty, duplicated or padded with whitespace. Same section as
above.

## Prompts and templates

A `prompt_file` holds the agent's turn prompt. These values are available,
and nothing else:

- `{{ task.title }}`, `{{ task.input }}`
- `{{ stages.<stage> }}` for a `capture: text` stage
- `{{ stages.<stage>.<field> }}` for a `capture: json` stage. A reviewer
  stage has `.summary` and `.outcome`; a script's JSON fields are available
  by name.
- `{{ arrival.from }}`, `{{ arrival.outcome }}`

They are filled in in: the contents of a `prompt_file`, an inline `command:`
of a shell stage, poll stage or watcher, and every `env:` value. They are
not filled in in a `system_prompt_file` or a `script_file`. A reference to
an unknown stage, or to a stage without `capture:`, is rejected at load. A
value that doesn't exist yet renders empty. A literal `{{` can't be written
in these places.

## Shell stages

- `command` runs with `sh -c` in the task's working directory. `script_file`
  is resolved next to the YAML.
- `env` names use letters, digits and `_`, don't start with a digit, and
  don't start with `CHOCO_`.
- `timeout` is optional.
- `on:` needs `done`. A non-zero exit gives `error`.
- Text an agent wrote reaches a script through `env:`, never inside
  `command:`, because a `command:` is templated as raw shell text.

## Roles

A role has `cli`, `model` and `system_prompt_file`. Other fields:

- `read_only: true` with `disallowed_tools: [edit, write, notebook_edit]`
  (those three names only; `read_only` needs all three and a workflow-level
  `worktree: true`). See [read-only roles](https://github.com/itsypkin/ChocoFactory/blob/main/docs/workflows.md#read-only-roles).
- `inherit_operator_config`, `skills` and `memory` control what the agent
  inherits from your Claude setup. `skills` and `memory` can't go with
  `inherit_operator_config`. See
  [what an agent inherits](https://github.com/itsypkin/ChocoFactory/blob/main/docs/workflows.md#what-an-agent-inherits-from-your-claude-setup).

More: [how a role is configured](https://github.com/itsypkin/ChocoFactory/blob/main/docs/models.md#how-a-role-is-configured),
[running a role on omp](https://github.com/itsypkin/ChocoFactory/blob/main/docs/models.md#cli-omp).

## Adding a check: an example

A shell stage between the review and the PR. In `coding-task`, change
`internal_review`'s `approved: open_pr` to `approved: lint`, and add:

```yaml
  lint:
    kind: shell
    command: cargo fmt --check
    timeout: 5m
    on: { done: open_pr, error: escalate_to_human }
```

A failed check goes to a human. Routing `error` back to the coder would not
work unless you also edit the coder's revise prompt (and so eject `prompts/`)
to handle a lint failure and give the stage `capture: text` so the prompt can
show `{{ stages.lint }}`; otherwise the coder never sees the failure.

A shell stage passing agent text on:

```yaml
  notify:
    kind: shell
    command: printf '%s\n' "$REVIEW_SUMMARY" > /dev/null
    env:
      REVIEW_SUMMARY: "{{ stages.internal_review.summary }}"
    on: { done: open_pr }
```

A poll stage with backoff:

```yaml
  wait_deploy:
    kind: poll
    command: curl -fso /dev/null https://example.com/health && echo ready
    interval: 30s
    backoff:
      - { after: 10m, interval: 2m }
    timeout: 1h
    outcomes:
      - match: '\Aready'
        then: ready
    on: { ready: open_pr, timeout: escalate_to_human }
```

A gate that waits for `/ship`:

```yaml
  sign_off:
    kind: human_gate
    capture: text
    markers:
      - line: /ship
        then: shipped
    on: { shipped: open_pr }
```

A reply without the `/ship` line is refused and nothing is sent, so the
gate has no way to send work back. A reviewer who wants changes can't reply
with text; they cancel the task. To give the gate a second route, add
another marker with its own `then`.

A second reviewing role and stage:

```yaml
roles:
  auditor:
    cli: claude
    model: claude-opus-5-5
    system_prompt_file: prompts/reviewer-system.md
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
```

```yaml
  audit:
    kind: agent_turn
    role: auditor
    prompt_file: prompts/reviewer-turn.md
    capture: json
    report_sections: [Reviewed, Findings]
    on: { approved: open_pr, changes_requested: escalate_to_human }
```

`changes_requested` goes to a human, not to `revising`. The built-in revise
prompt (`prompts/coder-revise.md`) has no entry for an arrival from `audit`
and never shows `{{ stages.audit.summary }}`, so a coder sent there would
revise without the audit's findings. To route to `revising` anyway, eject
`prompts/`, add an `audit` case to the revise prompt that shows
`{{ stages.audit.summary }}`, and add a `loop_guard`.
