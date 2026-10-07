# Writing the spec: detail

## Contents
- Check that the design holds
- Having choco check the spec first (`coding-task-planned`)
- Which `--help` to trust
- Tasks that read an external API
- Tasks whose coder runs a real third-party CLI
- Docs-only tasks

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

A docs task has no tests, so "the tests are a floor" doesn't apply. The
done criteria are instead:

- every link and anchor resolves (relative links between files, `#anchors`
  by GitHub's slug rules, and links into other docs);
- every command and flag shown matches `--help` from a build of the target
  branch (see above);
- no fact from the old text is lost: the PR lists the facts it moved and
  where each went.

Check these yourself before merging a docs PR; a script that walks the
links and a loop over the `--help` of each command are enough.
