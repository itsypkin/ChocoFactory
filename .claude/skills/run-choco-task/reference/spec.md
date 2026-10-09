# Writing the spec: detail

## Contents
- Check that the design holds
- Having choco check the spec first (`coding-task-planned`)
- Which `--help` to trust
- Tasks that read an external API
- Tasks whose coder runs a real third-party CLI
- Docs-only tasks
- Text an agent or a reader acts on
- Running a role on omp

## Check that the design holds

Check that the design holds, not only that it builds. For a feature
that enforces or protects something, list every way the protected event
can end, including a fresh retry, a resumed retry and the daemon's
restart sweep, and say whether the protection runs on each. Mark each
blind spot of the check (content vs status, ignored paths, a mistyped
config key) as covered or as an accepted cost. Say which condition each
prescribed message is true under. For a layout, list its invariants and
require one test that renders every fixture at every size from the
minimum up. Name one test for each fail-closed path in the done
criteria.

Three more checks belong in the spec:

- **Every statement the change makes false.** Grep the skills, docs and
  prompts for the old behaviour's wording and commands, and fix each hit
  in the same PR.
- **Abandoned operations.** For anything that can time out, be cancelled,
  or lose its client, the spec states what is left behind (locks, rows,
  files) and requires a test that interrupts it and checks that state.
- **A new rule in a prompt.** The spec requires the coder to run one
  probe of the rule on the role's production model (the model the role
  runs in the workflow) in its lap, and to report the probe and its
  result.

## Having choco check the spec first (`coding-task-planned`)

Create the task with `--workflow coding-task-planned`. A planning agent
checks the spec against the code the task starts from, fixes stale
references and loose test requirements, checks that the design holds on
every path, and decides the design choices your intent implies, listing
each with its reason. It parks the task at `spec_questions` with questions
for you only when it can't go on without guessing what you want: the spec
contradicts itself, the goal is unclear, or the only way forward is
irreversible, weakens security or costs far more than the spec suggests.
From then on, the coder and the reviewer work from its report, not your
`--prompt`. Read the report with
`choco --json task status <id> | jq -r '.workflow_state.payload.stages.spec_check.summary'`,
and answer with `choco task send <id> --text "<answers>"`. The planner
folds your answers in and checks again; an answer can tell it to decide a
question itself.

**Unverified claims.** The planner backs a claim about runtime behaviour
with the command it ran and the output. A claim it couldn't run read-only
is marked **unverified**, with a probe for the coder to run. Don't read
every Decision as checked: treat an unverified one as open until the coder
reports the probe's result.

## Which `--help` to trust

The installed `choco --help` matches the daemon you run. When a task
documents or changes the CLI on the default branch, build that branch
(`cargo build -p choco`) and use its `--help`
(`./target/debug/choco <command> --help`).

## Tasks that read an external API

Name the exact endpoints and fields the code reads. Check them read-only on
a real object before you write the spec, and paste the observed values
into it. Checking this way routinely turns up facts that decide the design
(a field that is absent, paged, or named differently from its docs).

## Tasks whose coder runs a real third-party CLI

A coder that runs a real CLI spends real money and touches a real account.
The spec must:

- set a budget: how many real calls, and which cheap model;
- reuse the operator's existing login, with no new login, profile or
  config;
- say what telemetry or account state the CLI may touch;
- require tests that run the real CLI to be opt-in, off in the default
  test run.

## Docs-only tasks

A docs task has no tests of its own, so its tests are the acceptance runs
of [Text an agent or a reader acts on](#text-an-agent-or-a-reader-acts-on),
and "the tests are a floor" applies to them. The done criteria are:

- every link and anchor resolves (relative links between files, `#anchors`
  by GitHub's slug rules, and links into other docs);
- every command and flag shown matches `--help` from a build of the target
  branch (see above);
- no fact from the old text is lost: the PR lists the facts it moved and
  where each went;
- for a skill or doc meant to be copied, its links resolve from a copy
  outside the repo, and every route of every snippet has been taken on a
  throwaway daemon;
- the acceptance runs below have been run and reported.

**A skill or doc meant to be copied.** Check its links from a copy
outside the repo, made the way its install instructions make it. Take
every route of every snippet on a throwaway daemon. A snippet that only
loads (parses, prints `--help`) has not been taken.

```bash
t=$(mktemp -d)
HOME=$t choco server start --port 0     # its own lock, database and log under $t
HOME=$t choco server status             # and the same HOME=$t on every choco command
HOME=$t choco server stop
```

The throwaway daemon's agents get the same `HOME`, so they have no
`claude` or `gh` login. A route that needs an agent turn or GitHub is
taken on a small, real task on your own daemon instead, without stopping,
updating or restarting that daemon.

A script that walks the links and a loop over the `--help` of each
command check the first three criteria only; they are not enough for
text meant to be copied or acted on.

## Text an agent or a reader acts on

For a prompt, a skill or docs, the spec carries behavioural acceptance
criteria:

- Following the text leads no agent or first-time reader into a hang, a
  poll loop, the wrong tree or a wrong action.
- Each new rule's way out restates the constraint it relaxes, and has a
  path for when the agent can't comply.
- Each command the text names exists for that agent. An agent driven by a
  daemon runs the `choco` on the `agents' choco` line of
  `choco server status`.
- The spec names the runs that prove it, and a coder's "not re-run" on any
  of them is an unmet requirement.

## Running a role on omp

`--role-cli reviewer=omp` (or `cli: omp` on the
role in the workflow) runs that role on the `omp` CLI, for example with
`--role-model reviewer=openai-codex/gpt-5.6-terra`. It uses your existing
omp login and needs no extra step. An omp role sees the repo's root
`CLAUDE.md` and `AGENTS.md` and `.omp/AGENTS.md` and `.omp/RULES.md`, plus
the repo's `.claude/CLAUDE.md` (loaded by omp itself), and
nothing from nested folders, folders above the repo, or your personal setup,
so put what it needs in those files or in the spec. Outside a `worktree:
true` workflow it can read but its edits and commands are refused. It can't
use `memory: true`. A role on an Anthropic model needs an
`ANTHROPIC_API_KEY`, not a Claude subscription login.
For an existing task, `choco task reconfigure` takes effect on the next
turn.
