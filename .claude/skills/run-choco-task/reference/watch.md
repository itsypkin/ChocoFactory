# Watching a task: detail

## Contents
- Waiting with `--until`
- Reading a stage's verdict text
- Watching commits in a revise lap
- Stage times
- The workflow-file line

## Waiting with `--until`

`choco task status <id> --until attention --timeout <dur>` waits for every
stop that needs you: the task is open at a `human_gate` stage
(`spec_questions`, `awaiting_human_review`, `escalate_to_human`, or any gate
a custom workflow defines), or it is stuck, cancelled or closed. It looks at
the current state only, so started while the task already sits at a gate it
returns at once. The exit code says how it ended:

| Exit | Meaning under `attention` |
|---|---|
| 0 | waiting for you at a gate; stderr names the stage |
| 1 | error (unknown task, daemon unreachable) |
| 3 | `stuck` |
| 4 | cancelled |
| 5 | `--timeout` elapsed |
| 6 | closed |

After a PR vote (a `/approve` or `/request-changes` comment) the task stays
at `awaiting_human_review` until the daemon's poll reads the comment, up to
a minute. A fresh `--until attention` would return at once, so wait for the
stage to change first, for example
`choco task status <id> --until stage:revising --timeout 5m`, then wait on
`attention` again. A `choco task send` reply moves the task off the gate
before the command returns, so `--until attention` can follow it directly.

## Reading a stage's verdict text

The latest lap's text of `spec_check` or `internal_review`:

```bash
choco --json task status <id> | jq -r '.workflow_state.payload.stages.internal_review.summary'
```

`scripts/tail-events.sh` cuts each event to one line, so it can't show
reasons. Every lap's text is in the events. A report is a `tool_call` event
whose `payload.tool` ends in `report_outcome`, with `payload.input.outcome`
and `payload.input.summary`. Events come oldest first, 100 a page by default
and at most 500 with `--limit 500`; `next_token` (pass it to `--after`) is
null on the last page. This loop prints every report:

```sh
id=<task-id>
(
  token=
  while :; do
    page=$(choco --json task events "$id" --limit 500 ${token:+--after="$token"}) || exit 1
    printf '%s\n' "$page" | jq -r '.events[]
      | select(.event_type == "tool_call" and (.payload.tool | endswith("report_outcome")))
      | "== \(.created_at[0:19]) \(.payload.input.outcome)\n\(.payload.input.summary)\n"' || exit 1
    token=$(printf '%s\n' "$page" | jq -r '.next_token // empty') || exit 1
    [ -n "$token" ] || break
  done
)
```

These are every role's reports, not only the reviewer's: `ready` or
`needs_input` come from `spec_check`, `done` from `coding` or `revising`, and
`approved` or `changes_requested` from `internal_review`. Match their times
against the `Progress` table in `choco task status`. Use `printf '%s\n'`,
not `echo`: zsh's `echo` corrupts the JSON.

## Watching commits in a revise lap

`git fetch` of the task branch shows only what has been pushed. The branch
`task/<task-id>` is a local branch of the project's repo (`REPO` in
`choco project list`), and the task's worktree is a linked worktree of the same repo, so the
branch's commits show before they are pushed:

```bash
git -C <project-repo> log --oneline origin/main..task/<task-id>
git -C <project-repo> worktree list --porcelain | grep -B2 'refs/heads/task/<task-id>'
```

The second prints the task's worktree path on the `worktree <path>` line two
lines above the `branch` line.

## Stage times

`choco task status <id>` has a `Progress` table with the time of every
transition, so you needn't log them. Typical durations, observed on a
mid-sized repo with about 5 minutes of CI (ranges, not guarantees), for
choosing `--timeout`:

| Stage | Observed |
|---|---|
| `spec_check` | 2–9 min |
| `coding` | 8–42 min |
| `internal_review` | 1–9 min a lap that rejects; an approving lap takes about 5–10 min more, because it runs the gate (longer under load) |
| `revising` | 1–23 min |
| `open_pr` | seconds |
| `checks_polling` | your CI's time plus a minute |
| create → `awaiting_human_review` | 20–100 min |

## The workflow-file line

For a task run from a workflow file (`--workflow <path>`), the first
`Workflow` line in `choco task status` shows the YAML's `name:`, which reads
like a built-in. The next line, labelled `Workflow file`, shows the file's
path (with `(changed since task start)` or `(missing)` when that applies)
and its hash. A built-in shows `Workflow  builtin:<name>@<version>` there
instead.
