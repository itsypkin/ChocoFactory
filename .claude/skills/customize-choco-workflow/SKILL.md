---
name: customize-choco-workflow
description: Helps change how a ChocoFactory (choco) workflow runs on your own repo — different stages, roles, models, prompts or checks. Picks the lightest place for the change (a per-task override, a workflow file of your own, or a workflow committed in the repo), explains what ejecting the built-ins costs, and gives the rules for editing a workflow safely. Use when asked to change a choco workflow's stages, roles, models, prompts or checks, or to choose between a per-task override, a workflow file and a repo workflow.
---

# Customise a choco workflow

This is for people who use choco on their own repos. Pick the lightest
option that does the job, and try it on one task before you commit it.

## 1. Choose where the change lives

| Option | Use it when | It can't | It costs |
|---|---|---|---|
| **1. Per-task flags** | Only a role's model, CLI or system prompt changes, for one task. | Change stages, routing or turn prompts. | Nothing. No file changes. |
| **2. Your own workflow file** | You want to try a changed stage or prompt, or run a personal variant, without touching the repo. | Reach teammates or other tasks. | A file to keep. A live-read caveat (below). |
| **3. A repo workflow** | The team should share the change for every task in that repo. | Keep following choco updates for the names you edit. | An eject (section 2). |

### Option 1: per-task flags

On `choco task create`, and on `choco task reconfigure <id>` for a task that
already exists:

- `--role-model ROLE=MODEL`
- `--role-cli ROLE=CLI`
- `--role-system-prompt ROLE=TEXT`
- `--role-system-prompt-file ROLE=PATH`
- `--config '<json>'`, for anything the typed flags don't cover

Each role flag is `ROLE=VALUE` and can repeat. `ROLE` is a key of the
workflow's `roles:`: `coder` and `reviewer` in `coding-task`, plus `planner`
in `coding-task-planned`. A name that isn't in `roles:` applies to nothing.

```bash
choco task create --project myproj --workflow coding-task \
  --title "…" --prompt "…" --role-model reviewer=sonnet
```

- `reconfigure` takes effect on the next turn. A session that is already
  running keeps the config it started with.
- The system-prompt flags replace the role's whole system prompt from the
  workflow. They don't add to it, so an override of the reviewer's drops the
  built-in review rules. The turn prompts don't change.
- They can't change stages, routing, prompts other than the system prompt,
  or the role fields only a workflow can set: `read_only`,
  `disallowed_tools`, `skills`, `memory`, `inherit_operator_config`. Task
  config ignores those keys.
- The machine-wide `~/.config/chocofactory/config.yaml` ranks below a
  workflow's `roles:`. The built-ins set the CLI, model and system prompt
  of every role, so it doesn't change a built-in role.

More: [per-role flags](../../../docs/cli.md#per-role-flags-and-changing-a-tasks-config),
[how a role is configured](../../../docs/models.md#how-a-role-is-configured).

### Option 2: a workflow file

`choco task create --workflow <path>.yaml`. A value that contains `/` or ends
in `.yaml` or `.yml` is a path. Its prompts and scripts resolve next to it.

**Caveat:** a task started from a `--workflow` file reads that file's
prompts and scripts from that checkout while it runs. Don't edit those
files, and don't switch branches in that checkout, while a task from it is
running. This holds for the YAML too.

`choco task status` marks the `Workflow file` line `(changed since task
start)` only when the YAML file itself changes, and `(missing)` when it is
gone. It doesn't flag edits to the prompt and script files, or a branch
switch that leaves the YAML's bytes the same.

### Option 3: a repo workflow

`choco project init-workflows <project>` (the project must have a repo)
copies the built-ins into `<repo>/.chocofactory/workflows/`. Edit them,
then commit `.chocofactory/`.

A task started by name from a repo workflow reads these files from the
project's own checkout while it runs. The caveat above applies to that
checkout too.

## 2. The cost of ejecting

Once the repo has `.chocofactory/workflows/<name>.yaml`, that file wins over
the built-in of that name. Choco updates stop reaching it and its prompts
and scripts.

- **Eject only what you change.** `init-workflows` seeds all three
  built-ins (`chat`, `coding-task`, `coding-task-planned`) with their
  `prompts/` and `scripts/`. Delete the YAMLs you won't edit, so those names
  keep following the daemon. Keep `prompts/` and `scripts/` whole, because
  the YAMLs share them. Running `init-workflows` again never overwrites a
  file, but it re-creates the ones you deleted.
- **Commit twice.** First the seeded files, untouched, with the daemon
  version in the message (the first line of `choco server status`). The
  ejected files don't record it. Your edits go in later commits.
- Renaming an ejected file, say `coding-task.yaml` to `my-flow.yaml`, makes
  the built-in name resolve to the built-in again. Set `name:` inside to
  match.

Comparing with the current built-ins and bringing their changes across:
[reference/ejecting.md](reference/ejecting.md).

## 3. Edit safely

The stage kinds and their keys, routing, loop guards, poll and watch
timing, templates, shell stages and roles are in
[reference/editing.md](reference/editing.md). Read it before you edit a
stage. The short version: a misspelt stage key or role field is rejected
when the workflow loads, but a misspelt top-level key is silently ignored.

## 4. Try a change first

1. Run one task with `--workflow <path>` before you commit.
2. Read `choco task status <id>`. For a file, the `Workflow file` line shows
   the path and `[hash]` the task actually runs, with `(changed since task
   start)` or `(missing)` when it changed or is gone. For a built-in, a
   second `Workflow` line reads `builtin:<name>@<version>`. The first
   `Workflow` line is the YAML's `name:`, so check the path line.
3. `--workflow` takes the workflow from wherever the file is. The task's code
   still forks from `--repo`'s checkout (default: the project's repo) and
   `--base`. To try a repo-workflow change on a branch, point `--workflow` at
   a separate checkout or `git worktree` of that branch.
4. To wait for every stop that needs you, run
   `choco task status <id> --until attention --timeout <dur>`. It returns at
   any open `human_gate` stage, including ones you added, and when the task
   is stuck, cancelled or closed. Exit codes: 0 at a gate, 3 stuck, 4
   cancelled, 6 closed, 5 timed out. See
   [watching a task](../../../docs/cli.md#watching-a-task).

Every agent stage runs the real CLI and costs money. Try it on a small, real
change.

## 5. Security

A repo's `.chocofactory/workflows/` and any `--workflow` file can run shell
commands as you: shell stages, poll and watch commands, and scripts. They
run with the daemon's environment. Treat `.chocofactory/` like a Makefile
or CI config, and review changes to it like code. See
[Security](../../../docs/workflows.md#security).

More on where a workflow comes from:
[project workflows](../../../docs/workflows.md#project-workflows),
[which workflow file a task ran](../../../docs/workflows.md#which-workflow-file-a-task-ran),
[customising workflows](../../../docs/workflows.md#customising-workflows).
