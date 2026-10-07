# ChocoFactory

ChocoFactory runs AI coding agents as supervised workflows: an agent writes the change, a second agent reviews it, CI runs, and you give the final verdict on the pull request. You can hand it a spec and walk away. It is driven by the `choco` command line, backed by a background daemon, `chocofactoryd`.

Jump to [Install](#install), or see the [Documentation](#documentation) list.

## The problem

Handing a coding task to an AI agent and walking away goes wrong in familiar ways:

- **It stalls.** The agent waits on something, such as a test run, a timer or an answer, and never comes back.
- **It wanders off the spec.** Nothing checks the result against what you asked for.
- **It reviews its own work and approves it.** The same agent that wrote the change signs it off.
- **It loses its place.** A restart, a crash or a closed laptop ends the session, and the work is gone or half done.
- **It needs a human watching every step** to catch all of the above, which defeats the point.

## How ChocoFactory solves it

- **Workflows as explicit stages.** The built-in `coding-task` workflow goes: code → internal review → open a PR → wait for CI → human review → done. Every way back (a rejected review, red CI, your `/request-changes`, a resumed escalation) goes through a revise stage, so there is one path for fixing things.
- **A separate reviewer.** A different agent, in its own session and read-only, reviews the change against the spec before any PR exists. Read-only is enforced: the daemon checks that the worktree didn't change.
- **Loop guards and escalation.** Repeated rejections, repeated red CI, or no verdict from you for six hours park the task for a human instead of looping forever. A turn that stops reporting is nudged, then marked stuck.
- **Tasks survive restarts.** Everything lives in the daemon's database. Waits survive a restart. Interrupted agent turns are parked and can be retried, resuming the agent's session when possible.
- **Isolation from your personal setup.** On the coding workflows, agents don't see your `~/.claude/CLAUDE.md`, plugins, hooks, MCP servers or memory. They see the repo's own instruction files. (The built-in `chat` workflow deliberately inherits your setup.) Each task works in its own git worktree.
- **Your verdict lives on the PR.** Comment `/approve` or `/request-changes` on the pull request.
- **One dashboard** (`choco dashboard`) for every task: what needs you, what's running, what's stuck.
- **Checking the spec first.** `coding-task-planned` puts a planning agent in front. It checks the spec against the code and asks you only when it must.
- **Multi-model support.** Each role resolves its own CLI and model, so the coder and the reviewer can run different models. Roles run on Claude Code by default. A role can run on [omp (oh-my-pi)](https://github.com/can1357/oh-my-pi) instead, which reaches other providers' models. For example, a Claude coder with an OpenAI GPT reviewer, using your existing omp login:

  ```
  choco task create ... --role-cli reviewer=omp --role-model reviewer=openai-codex/gpt-5.6-terra
  ```

  This has been verified with OpenAI models over omp's OAuth login. An Anthropic model *through omp* needs an Anthropic API key, not a Claude subscription login; see [`cli: omp`](docs/models.md#cli-omp).

## An example

One task, from issue to merged PR. The repo `myapp` and issue 42 are made up.

```
choco server start
choco project create myapp --repo ~/code/myapp
choco task create --project myapp --workflow coding-task \
  --title "Fix the login redirect (#42)" --prompt "$(cat spec.md)"
choco dashboard
gh pr list --head task/<id>
gh pr comment <number> --body "/approve"
gh pr merge <number> --squash
```

1. Start the daemon.
2. Register your repo as a project.
3. Create the task. It prints the task's id. The spec in `spec.md` is the agent's whole brief, so make it a real spec, not one line.
4. Watch it with `choco dashboard`, or block until it needs you: `choco task status <id> --until stage:awaiting_human_review --timeout 2h`.
5. Find its PR. The branch is named `task/<id>`.
6. Approve it by commenting `/approve` on the PR (or use the GitHub web UI to comment). The task moves to done.
7. Merge it, with any merge method. Merging is still yours to do (merging on its own also counts as approval).

Because the title ends in `(#42)`, the PR body says `Closes #42`, so merging closes the issue.

## Install

```
curl -fsSL https://github.com/itsypkin/ChocoFactory/releases/latest/download/install.sh | sh
```

- Installs `choco` and `chocofactoryd` into `~/.local/bin`.
- Platforms: macOS arm64 and x86_64, Linux x86_64 and aarch64.
- The two binaries always live **side by side** in one directory: `chocofactoryd` hands agents the `choco` next to it, and `choco server start` runs the `chocofactoryd` next to it.
- The installer's environment variables, checksum verification and installing from source are in [docs/cli.md](docs/cli.md#updating).

What you need first:

- The **`claude` CLI**, logged in. Every agent turn runs it, so tasks cost real money.
- **`gh`**, authenticated as the account that opens the PRs.
- **`git`**.

To update, run `choco update`. `choco update --check` only reports whether an update is available. A running daemon is restarted on the same port; the built-in workflows update with the binary.

## Quick start

```
choco server start                          # background daemon; waits until it answers
choco server status                         # version, pid, port, open tasks
choco project create <name> --repo <path>   # register a repo
```

Then create a task as in the example above, and watch it in the [dashboard](#the-dashboard). The full walkthrough (statuses, cost and time, messages, cancelling, events) is in [docs/cli.md](docs/cli.md#a-full-walkthrough). If a task gets stuck, see [Stuck tasks](docs/cli.md#stuck-tasks).

## The dashboard

`choco dashboard` (alias `choco dash`) is an interactive terminal view of every task. Its four sections:

| Section | Holds |
|---|---|
| Needs you | tasks waiting at a human gate, such as your PR verdict |
| In progress | every other open task |
| Stuck | tasks the engine could not move forward, with the reason |
| Recently closed | the latest finished and cancelled tasks |

Keys for day one:

| Key | Action |
|---|---|
| `⏎` | open the selected task's detail view |
| `o` | open the task's pull request |
| `r` | retry a stuck task |
| `c` | cancel a task |
| `?` | list all keys |
| `q` | quit |

The full key table, columns and behaviour are in [docs/cli.md](docs/cli.md#the-dashboard).

## Reviewing a PR

When a `coding-task` reaches `awaiting_human_review` it has already pushed a branch, opened a PR and waited for CI. It wants a verdict from you, and it reads that from the PR's **comments**, not from GitHub's formal review button.

Leave a PR comment containing one of these markers, **alone on its own line**, with your review above it:

| Marker | Effect |
|---|---|
| `/approve` | the task moves to `done` |
| `/request-changes` | the task goes back to `revising`, and the coder gets your comment |

- Only comments newer than the head commit count, so there is nothing to clear between rounds.
- Only comments from the repo's owners, members and collaborators vote.
- Merging the PR counts as approval too.
- `choco task send <id> --text ...` is the other channel: it answers without touching the PR, with the same markers.

The full rules (editing a comment, code blocks, the six-hour window, escalation counts, CI polling, how the PR body is written) are in [docs/coding-task.md](docs/coding-task.md#reviewing-a-coding-task-pr).

## Customising workflows

A task's workflow comes from the first of these that matches:

1. an explicit path: `choco task create --workflow <path-to.yaml>`;
2. the project repo's `.chocofactory/workflows/<name>.yaml`;
3. the built-in of that name, which ships inside the daemon.

`choco project init-workflows <project>` copies the built-ins into the project's repo as a starting point, so a team can version its own stages, roles, models and prompts next to the code.

**Trust warning:** a repo's workflows, and any `--workflow` file, can run shell commands as you. Only point choco at repos and files you trust.

More in [docs/workflows.md](docs/workflows.md).

## Using choco from Claude Code

The `run-choco-task` skill teaches Claude Code to drive a `coding-task`
end to end: write the spec, watch the task, review its PR and recover it.
Copy the whole skill folder, including `scripts/` and `reference/`, into
your repo's `.claude/skills/` (or `~/.claude/skills/` for every repo). From
the repo's root:

```bash
dest=.claude/skills    # or ~/.claude/skills for every repo
ver=$(choco --version | awk '{print $2}')
d=$(mktemp -d) &&
  git -c advice.detachedHead=false clone --depth 1 --filter=blob:none --sparse --branch "v$ver" https://github.com/itsypkin/ChocoFactory.git "$d" &&
  git -C "$d" sparse-checkout set .claude/skills/run-choco-task &&
  mkdir -p "$dest" && rm -rf "$dest/run-choco-task" &&
  cp -R "$d/.claude/skills/run-choco-task" "$dest/" &&
  rm -rf "$d"
```

- `--branch "v$ver"` takes the skill from the release you run. The skill
  describes that release only, so fetch it again after `choco update`.
  Without `--branch` you get `main`'s copy, which can describe behaviour
  newer than your choco.
- A clone keeps `scripts/tail-events.sh` executable. If you fetch the files
  another way, such as the GitHub contents API, `chmod +x` it.
- A new Claude Code session finds the skill. In a running session, if the
  `skills/` folder you copied into didn't exist when it started, run
  `/reload-skills` or start a new session.

## Documentation

- [docs/cli.md](docs/cli.md): the `choco` CLI: flags, the daemon, updating, a full walkthrough, stuck tasks, scripting, watching tasks, the dashboard.
- [docs/coding-task.md](docs/coding-task.md): the built-in coding workflows, PR review rules, escalation limits, CI polling and the PR body.
- [docs/workflows.md](docs/workflows.md): writing and customising workflows: where they come from, routing on a verdict, isolation, read-only roles, human gates.
- [docs/models.md](docs/models.md): roles, CLIs and models, including running a role on omp.
- [CONTRIBUTING.md](CONTRIBUTING.md): building, tests, running the daemon by hand, environment variables, releasing.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for building from source, running the tests and releasing.

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
or MIT license ([LICENSE-MIT](LICENSE-MIT)) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or conditions.
