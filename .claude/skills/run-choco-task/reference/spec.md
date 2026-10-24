# Writing the spec: detail

## Contents
- Check that the design holds
- Stop conditions
- Having choco check the spec first (`coding-task-planned`)
- Which `--help` to trust
- Tasks that read an external API
- Tasks whose coder runs a real third-party CLI
- Docs-only tasks
- Text an agent or a reader acts on
- Running a role on omp
- Probing what an omp role sees

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

A spec also says what the change makes untrue elsewhere (docs, prompts,
skills), what an operation that is interrupted leaves behind, and which
test shows that each rule the change adds actually matters. Its
out-of-scope list never forbids the tests review will ask for: a task
that can't add them ends at `escalate_to_human`, so for a script, say
where its tests live. Your
project's instruction files (`CLAUDE.md`, `AGENTS.md`) may name checks
of their own; carry those into the spec too.

**Every run the spec requires has a budget and a way out.** The spec
states how many runs, and what the coder does when it can't make one (no
access to the model or provider, no login, budget spent): it reports the
run as not made, with the reason, and still finishes: a check that can't be
made is not a blocker (see [Stop conditions](#stop-conditions)), so it
doesn't stop and wait. Its summary and the PR
description mark the item "not made: <reason>". The spec also tells the
internal reviewer, which runs inside the task, not to request changes for
that item but to name it in its report so it reaches the human gate; the
reviewer still blocks on everything else, and on a not-made item with no
reason or outside the spec's way out. The coder never skips a run
silently and never retries in a loop.

## Stop conditions

A coder can stop and wait for you. Its
`coding` and `revising` stages have a second exit, `blocked`, which parks the
task at `blocker`. The coder uses it only when the task tells it to stop in the
situation it is in, or when every way forward breaks a rule the task sets.
Ambiguity is not a reason: it takes the most reasonable reading. So for any
rule the coder must not work around (pinned versions, no new dependencies, a
file it must not touch), write the stop condition next to the rule, in the form
"if X, don't do Y; stop and report `blocked`", and name what to report:

> Pin `serde` to `=9.9.9`. If it doesn't resolve, don't substitute another
> version or crate; stop and report `blocked`, with the exact error, the
> command that printed it and the decision you need.

The report is your whole briefing, and the coder is told to put in it what
stops it, the commands it ran and what they printed, the ways forward it
considered and which rule each breaks, and the decision it needs. Read it with
`choco --json task status <id> | jq -r '.workflow_state.payload.stages | (.revising // .coding).summary'`, and answer
at `blocker` (see [recover.md](recover.md#blocker-answer-or-cancel)). Your
answer wins over the spec where they conflict, for the coder and for the
internal reviewer. A check the coder can't run (no network, no login) is not a
blocker: the spec still says to report it as "not made: <reason>" and finish.

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

`choco task send` is refused while `spec_check` runs. To change the spec
while it runs, cancel the task and create it again with the new spec.

**Unverified claims.** The planner backs a claim about runtime behaviour
with the command it ran and the output. A claim it couldn't run read-only
is marked **unverified**, with a probe for the coder to run. Don't read
every Decision as checked: treat an unverified one as open until the coder
reports the probe's result.

### Text that must arrive word for word

The planner may reword, reflow or shorten what it carries over, for a Claude
coder and an omp coder alike, so on this workflow "in the spec" means "in what
the planner carries over". Put text whose exact wording matters (versions and
features, a full argument list, rules a file must contain word for word) in a
fenced block, and write above it: "Carry this block word for word into the
Spec." Then check the report before coding starts:

```bash
choco task pause <id>      # while spec_check runs, not while it shows Queued
choco task status <id> --until paused --timeout 20m
choco --json task status <id> | jq -r '.workflow_state.payload.stages.spec_check.summary' > report.md
diff spec.md report.md     # each marked block must come through unchanged
choco task resume <id>
```

The pause lets the planner's turn finish and holds the task before `coding`
starts. If it stopped at `spec_questions`, answer, resume, and pause again the
same way. If a marked block came through changed, cancel the task and create
it again with the block restated: once coding starts, the coder has only the
report.

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
  throwaway daemon (a route that needs an agent turn or GitHub is taken by
  the operator, not the coder);
- the acceptance runs below have been run and reported.

**A skill or doc meant to be copied.** Check its links from a copy
outside the repo, made from the branch under test: copy the whole skill
or doc folder(s), in the layout the install instructions produce, from
the worktree into a new temp dir with `cp -R`. Copy every folder the
install copies, so a skill that links to another skill keeps working. Never run install steps
that fetch a release or `rm -rf` a folder, and never write inside the
repo or worktree. Take every route of every snippet on a throwaway
daemon; a snippet that only loads (parses, prints `--help`) has not been
taken.

The route below acts only on the daemon it started: `${t:?}` aborts if
`t` is empty or lost between shell calls (set `t` to the same literal path
in each later call; never run `mktemp` again),
`env -u` clears an exported `CHOCO_BASE_URL`, and each later command
needs the lock file, because without it `choco` falls back to the
operator's daemon.

```bash
t=$(mktemp -d)
env -u CHOCO_BASE_URL HOME="${t:?}" choco server start --port 0   # its own lock, database and log under $t
test -f "${t:?}/.config/chocofactory/chocofactoryd.lock" && env -u CHOCO_BASE_URL HOME="$t" choco server status
test -f "${t:?}/.config/chocofactory/chocofactoryd.lock" && env -u CHOCO_BASE_URL HOME="$t" choco server stop
rm -rf "${t:?}"    # also after a failed step: stop the daemon first if the lock exists
```

The throwaway daemon's agents get the same `HOME`, so they have no
`claude` or `gh` login. The coder opens no PR, and creates no task
outside its own throwaway daemon, to satisfy a criterion. A route that needs an agent turn or GitHub is taken
by the operator (or a named person who isn't the coder), on a small
real task on their own daemon without stopping, updating or restarting
it. The spec names that owner and a budget of runs; the coder reports the
route as "not made: <reason>" in its summary and the PR description and
finishes, so the item reaches that person at the human gate.

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
  of them is an unmet requirement unless the run was reported as not
  made, with the reason, under the run budget the spec states (see
  "Every run the spec requires has a budget and a way out" above).
- When the text is a skill or doc meant to be copied, the criteria
  include the copy-outside-the-repo check in
  [Docs-only tasks](#docs-only-tasks).
- For text an omp role reads, such as the repo's instruction files, a run can
  be one omp turn through your own daemon; see
  [Probing what an omp role sees](#probing-what-an-omp-role-sees).

## Running a role on omp

`--role-cli reviewer=omp` (or `cli: omp` on the
role in the workflow) runs that role on the `omp` CLI, for example with
`--role-model reviewer=openai-codex/gpt-5.6-terra`. It runs at thinking level
`high` unless the model ends in a level such as `:medium`. It uses your existing
omp login and needs no extra step. An omp role sees the repo's root
`CLAUDE.md` and `AGENTS.md` and `.omp/AGENTS.md` and `.omp/RULES.md`, plus
the repo's `.claude/CLAUDE.md` (loaded by omp itself), and
nothing from nested folders, folders above the repo, or your personal setup,
so put what it needs in those files or in the spec (on `coding-task-planned`,
in what the planner carries over; see
[Text that must arrive word for word](#text-that-must-arrive-word-for-word)). Outside a `worktree:
true` workflow it can read but its edits and commands are refused. `memory: true` works, but it is a separate memory per choco
project, kept under `~/.config/chocofactory/omp/memory/<project-id>/`, not
your Claude auto-memory. omp's own files for a choco turn (session registry, locks, logs, terminal pointer, client files) go to `~/.config/chocofactory/omp/xdg-state/omp/`; only `~/.omp/agent/agent.db` and `models.db` are still written ([details](../../../../docs/models.md)). A role on an Anthropic model needs an
`ANTHROPIC_API_KEY`, not a Claude subscription login.
For an existing task, `choco task reconfigure` takes effect on the next
turn.
To check what an omp role reads, see
[Probing what an omp role sees](#probing-what-an-omp-role-sees).

## Probing what an omp role sees

To check what an omp role reads from a commit's files, for an acceptance run
or to see which instruction files load, ask one omp turn on your own daemon:

```bash
${CLAUDE_SKILL_DIR}/scripts/omp-probe.sh <project> <repo> <commit> <model> "<question>"
```

`<commit>` must be in `<repo>`; for a PR, run `git fetch origin pull/<n>/head`
and pass `gh pr view <n> --json headRefOid -q .headRefOid`. The script exports
the commit with `git archive` into a temp folder outside any checkout and runs
one turn of the `omp-probe` workflow there, with no worktree, so the turn's
edits and commands are refused. It waits up to 10 minutes, prints the answer
and removes the folder, cancelling the task first if it hasn't closed. Because
the turn runs through choco, it loads what your daemon gives any omp role. It
differs from a coder or reviewer turn in two ways: it has choco's default role
system prompt, not the built-in role's, and your question is its whole prompt.
Pass the role's own model string, `:level` suffix included. To see which
instruction files load, put a random marker line in each (not derived from
the file's name), put one more in a file that must not load as a decoy, and
ask a question that doesn't list the markers. A file counts as loaded only if
its marker comes back and the decoy's doesn't. An acceptance question must
also fail on a control commit without the text.

It exits 3, after printing the answer, when the turn called a tool other than
`report_outcome`: the answer may then come from a file it read rather than
from what it loaded. Each probe is one agent turn on your omp login and leaves
a closed task in the project. It needs your own daemon: a throwaway daemon's
omp has no login. If the script is killed outright, the task isn't cancelled:
it finishes its one turn and closes itself. If `choco task create` is killed
after the daemon made the task but before it printed the id, the script can't
cancel it and removes the export folder, so the turn may fail and leave the
task open: find it with `choco task list` and cancel it.
