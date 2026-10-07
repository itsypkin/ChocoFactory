# Roles, CLIs and models

Every agent in a workflow runs as a named role, and each role chooses its own agent CLI and model. This page explains how a role's settings are resolved, the flags that set them for one task, and how to run a role on `omp` to reach other providers' models. Back to the [README](../README.md).

## How a role is configured

A workflow can declare more than one role — a `coder` and a `reviewer`, say —
and each resolves its own CLI, model and system prompt from three layers,
most specific wins, independently per field:

```
task config (--role-* below)  >  the workflow's roles: block  >  ~/.config/chocofactory/config.yaml
```

The `--role-*` flags set the task-level layer. Each is `ROLE=VALUE` and each
is repeatable, so several roles can be configured in one command. The built-in
`coding-task` has two roles, `coder` and `reviewer`:

```
$ choco task create --project acme --workflow coding-task \
    --title "fix the flaky test" --prompt "see issue 41" --repo ~/src/acme \
    --role-model coder=opus \
    --role-model reviewer=sonnet \
    --role-system-prompt-file reviewer=./strict-reviewer.md
```

The role names are whatever that workflow's `roles:` block declares — a name
that isn't in it is simply not applied to anything.

A role's `cli:` picks the agent adapter that runs it. The adapters are
`claude` (the default) and `omp`, described below. An unknown name is rejected rather than run as
`claude`: when a workflow is loaded, when the daemon starts (a bad `cli:` in
`config.yaml` stops it with the message), and when a task is created or
reconfigured (`--role-cli`). A value that slips in later, such as `config.yaml`
edited while the daemon runs, parks the task `stuck` when the turn starts. A
session can only be resumed by the adapter that created it, so a retry whose
role's `cli:` has changed starts fresh instead.

| Flag | Sets |
|---|---|
| `--role-cli ROLE=CLI` | which agent adapter runs that role (`claude` or `omp`) |
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


## `cli: omp`

`omp` is [oh-my-pi](https://github.com/can1357/oh-my-pi), a fork of the Pi
coding agent. A role with `cli: omp` runs on whatever models omp offers, such
as `openai-codex/gpt-5.6-terra`, so a workflow can put one role on a
non-Anthropic model:

```yaml
roles:
  reviewer:
    cli: omp
    model: openai-codex/gpt-5.6-terra:high
```

- **Login.** The role uses your existing omp login. choco takes no extra
  step: it never logs in, never creates a profile, and never reads or copies
  omp's credential files. Set `CHOCOFACTORY_OMP_BINARY` to use a different
  `omp` executable.
- **Anthropic models.** A role that runs omp with an Anthropic model must
  authenticate with an Anthropic API key (`ANTHROPIC_API_KEY`, from the Claude
  Console), not a Claude Free, Pro or Max login. Anthropic's terms allow a
  subscription login only in Claude Code and Anthropic's own apps; see the
  [Consumer Terms](https://www.anthropic.com/legal/consumer-terms) (section 3,
  item 7) and
  [Authentication and credential use](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use).
  Roles on `cli: claude` are not affected.
- **What a role sees.** An isolated role (the default) gets the model, the
  tools `read`, `bash`, `edit`, `write`, `glob`, `grep` and `todo` (minus any
  `disallowed_tools`), the skills its `skills:` list names, and the repo's own
  instruction files: the root `CLAUDE.md` and `AGENTS.md`, and
  `.omp/AGENTS.md` and `.omp/RULES.md`, each only if present. omp's own
  `claude` provider also loads the repo's `.claude/CLAUDE.md` (the repo's own
  file; there is no walk up to parent folders). It does not see
  anything from folders above the repo, your home folder, `~/.claude`, `~/.omp`
  or your own omp setup (settings that don't load instructions, tools or
  extensions, such as retry and compaction, still apply), and `.omp/mcp.json`
  is not loaded. Instruction files in subfolders (`sub/CLAUDE.md`) are not
  loaded either: a known gap. A role with `inherit_operator_config: true` (the
  chat role) keeps your omp setup and the repo's files, as it does on claude.
- **Outside a disposable worktree.** A role that doesn't run in a
  `worktree: true` checkout can read files but has its edits and commands
  refused, and the agent sees the refusal. A role in a worktree has full
  tool access there, like a claude role.
- **No memory.** `memory: true` on an omp role is rejected, with the role's
  name, when the workflow loads, when a task is created or reconfigured, and
  when a turn starts. A skill name containing `,` `*` `?` `[` `]` `{` or `}`
  is rejected for the same reason: omp would read it as a pattern.
- **Thinking level.** `medium`, unless the model string ends in a level, such
  as `:high` (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`,
  `auto` or `inherit`).
- **Cost.** omp's list price is recorded as the turn's cost. On a subscription
  login it is shown as the API-equivalent price, like a claude subscription's;
  for a model omp has no price for, the cost is unknown rather than zero.
  Usage is best-effort: a failed read never changes how a turn ends.
- **Telemetry.** No telemetry is sent: omp's QA reporting and OpenTelemetry
  export are switched off for every omp process choco starts.
