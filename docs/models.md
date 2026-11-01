# Roles, CLIs and models

Every agent in a workflow runs as a named role, and each role chooses its own agent CLI and model. This page explains how a role's settings are resolved, the flags that set them for one task, and how to run a role on `omp` to reach other providers' models. Back to the [README](../README.md).

## How a role is configured

A workflow can declare more than one role — a `coder` and a `reviewer`, say —
and each resolves its own CLI, model and system prompt from three layers,
most specific wins, independently per field:

```
task config (--role-* below)  >  the workflow's roles: block  >  ~/.config/chocofactory/config.yaml
```

`claude_settings` is the exception: it has two layers, `config.yaml` and the
workflow's `roles:` block, merged key by key, and no task layer (see
[`claude_settings`](workflows.md#claude_settings-claude-code-settings-for-chocos-agents)).

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
reconfigured (`--role-cli`). A role that resolves to `omp` and carries a
non-empty `claude_settings` is refused at the same points (workflow load, daemon
start, create, reconfigure, turn start), naming the role. A value that slips in later, such as `config.yaml`
edited while the daemon runs, parks the task `stuck` when the turn starts. A
session can only be resumed by the adapter that created it, so a retry whose
role's `cli:` has changed starts fresh instead.

| Flag | Sets |
|---|---|
| `--role-cli ROLE=CLI` | which agent adapter runs that role (`claude` or `omp`) |
| `--role-model ROLE=MODEL` | that role's model, optionally ending in `:<level>` (see [Effort level](#effort-level)) |
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


## Effort level

Every role runs at effort `high` unless its model string says otherwise. A
model string may end in `:<level>`, on both CLIs:

```
--role-model coder=claude-sonnet-5-5:xhigh
```

For a Claude role, choco strips the suffix and runs
`claude --model claude-sonnet-5-5 --effort xhigh`. The levels are `low`,
`medium`, `high`, `xhigh` and `max` (the ones `claude --help` lists for
`--effort`), written exactly like that: `HIGH` is refused. A level only omp
knows (`off`, `minimal`, `auto`, `inherit`) or one that doesn't exist (a typo
such as `hihg`, or an empty `claude-opus-5-5:`) is refused, naming the role:
when the workflow loads, when the daemon starts (a role in `config.yaml` with
`cli: claude` and a bad level stops it), and when a task is created or reconfigured (`--role-model`, or a
`--role-cli` that pairs a role with a model from another layer). A value that
slips in later, such as `config.yaml` edited while the daemon runs, parks the
task `stuck` naming the role when the turn starts. Model ids that contain a
colon themselves, such as Bedrock's `...-v1:0` or an ARN, are kept whole.
For an omp role see [Thinking level](#cli-omp).

**Cost.** `high` is now the default for every role, so turns are slower and
cost more than at the `medium` that Opus, Sonnet and Haiku 5.5 roles and omp
roles ran at before. Put `:medium` on a role to keep the old behaviour.

**What can override it.** The level reaches claude only as `--effort`.
choco removes `CLAUDE_CODE_EFFORT_LEVEL` from the daemon's environment before
starting an agent, because claude lets that variable beat the flag. An
`effortLevel` setting has no effect on claude 2.1.308 and can't beat the
flag either. Three things still can: an `env` block in the role's
[`claude_settings`](workflows.md#claude_settings-claude-code-settings-for-chocos-agents)
that sets `CLAUDE_CODE_EFFORT_LEVEL`; an `env` block that sets
`CLAUDE_CODE_EFFORT_LEVEL` in the task repo's own committed
`.claude/settings.json` (isolated roles apply the repo's committed settings,
not your user or local ones; see
[What an agent inherits](workflows.md#what-an-agent-inherits-from-your-claude-setup))
or in managed policy settings; and, for the chat role, the same `env` value
in your own user, project or local settings, since chat inherits your setup.

## Using choco with omp

To run a role on a model from another provider, run it on omp.

1. Install [omp (oh-my-pi)](https://github.com/can1357/oh-my-pi) and log in with the provider you want. choco uses that login as it is.
2. Pick the role and model for one task:

   ```
   choco task create --project myapp --workflow coding-task \
     --title "..." --prompt "$(cat spec.md)" \
     --role-cli reviewer=omp --role-model reviewer=openai-codex/gpt-5.6-terra
   ```

   Or set `cli: omp` and `model:` on the role in a workflow file, as in [`cli: omp`](#cli-omp).
3. Watch the task as usual. The roles you didn't change keep running on Claude Code.

omp has been verified with OpenAI models over its OAuth login. An Anthropic model *through omp* needs an Anthropic API key, not a Claude subscription login; the details are in [`cli: omp`](#cli-omp).

**omp as the reviewer.** An omp reviewer is a supported opt-in for code tasks; Claude stays the default. With `openai-codex/gpt-5.6-terra` it checks tests well, breaking the fix in a scratch copy to see a test fail, and matched the Claude reviewer on the one task measured so far. It costs nothing only on a free tier: the cost `choco task status` shows for it is the provider's list price, and an `openai-codex` login counts as a subscription, so the figure is the API-equivalent price, not what you paid. Set it for one task with `--role-cli reviewer=omp --role-model reviewer=openai-codex/gpt-5.6-terra`.

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
- **No `claude_settings`.** `claude_settings` are Claude Code settings; a role
  on omp can't take them and is refused (see above).
- **Outside a disposable worktree.** A role that doesn't run in a
  `worktree: true` checkout can read files but has its edits and commands
  refused, and the agent sees the refusal. A role in a worktree has full
  tool access there, like a claude role.
- **Memory.** `memory: true` works on an omp role, but it is a different
  memory from a claude role's. omp roles share one memory per choco project,
  kept by choco in `~/.config/chocofactory/omp/memory/<project-id>/` (the id
  is in `choco project list`). It stays until you delete that folder by hand:
  deleting the project removes it, but a project with any task can't be
  deleted and tasks are never deleted. The agent gets
  omp's `recall` and `retain` tools only; no embeddings are downloaded and
  the memory makes no model calls of its own. If omp can't open the
  store at startup, the turn fails naming the role and the store path. That
  check can't see every way memory can fail: a memory tool reporting "not
  initialised" means the turn ran without memory; if that matters,
  `choco task retry` re-runs the stage once the task is stuck.
  Memory roles of one project start one at a time so two starts don't open
  the store together. [What differs from claude](https://github.com/itsypkin/ChocoFactory/blob/main/docs/workflows.md#what-an-agent-inherits-from-your-claude-setup).
  A skill name containing `,` `*` `?` `[` `]` `{` or `}` is rejected, with the
  role's name, when the workflow loads, when a task is created or
  reconfigured, and when a turn starts: omp would read it as a pattern.
- **omp's own files.** For every omp process choco starts (isolated roles,
  the chat role, and `omp --version`), omp's bookkeeping files live in
  `~/.config/chocofactory/omp/xdg-state/omp/`, not in your `~/.omp`: the
  session registry, session locks, logs (`logs/`; look there for omp's log of
  a choco turn), the terminal "last session" pointer and the per-process
  client files. choco removes only a process's own client file when the omp
  process exits (the empty `run/daemons/<hash>/clients/` folders stay, one per worktree
  path; a daemon crash leaves that turn's file). It never removes the
  registry, locks, logs or pointer. The registry entries and locks stay as
  long as the transcripts in `~/.config/chocofactory/omp/sessions/`; omp
  prunes its own old logs; the pointer stays until omp overwrites it. A choco turn still writes
  `~/.omp/agent/agent.db` and `~/.omp/agent/models.db`: they hold your shared
  login and model list, and omp can't move them without moving the login.
  Side effects: commands the agent runs inherit `XDG_STATE_HOME`, so
  XDG-aware tools such as `gh` keep their state in choco's folder; and an
  operator cap `providers.maxInFlightRequests` no longer counts choco's turns
  against your own omp. A session started by choco 0.3.3 or earlier and
  resumed later gets a fresh lock in the new folder.
- **Your Claude MCP servers.** An isolated omp role pins `enabledProviders`
  to none and drops `CLAUDE_CONFIG_DIR` from its environment, so the MCP
  servers in your `~/.claude.json` don't start in its sessions. The chat role
  keeps both, as it keeps the rest of your setup.
- **Thinking level.** `high`, unless the model string ends in a level, such
  as `:medium` (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`,
  `auto` or `inherit`). omp judges its own model strings: a suffix that isn't
  one of these levels, such as `p/m:latest`, is part of the model id.
- **Cost.** omp's list price is recorded as the turn's cost. On a subscription
  login it is shown as the API-equivalent price, like a claude subscription's;
  for a model omp has no price for, the cost is unknown rather than zero.
  Usage is best-effort: a failed read never changes how a turn ends.
- **Telemetry.** No telemetry is sent: omp's QA reporting and OpenTelemetry
  export are switched off for every omp process choco starts.
