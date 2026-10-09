# Design: parallel stage groups

Status: **approved by the owner on 2026-10-09**. Inputs: `00-rough-idea.md`, the decisions in `01-idea.md` (Q2–Q9), and `02-research-single-stage-assumptions.md`. Tracking issue #257.

## 1. Overview

Today a task runs one workflow stage at a time. This design lets one stage start several stages at once and wait for all of them, before the workflow moves on to a stage that can read all of their results.

It is a general engine mechanism. Nothing in it is about reviewing. A review panel (security, architecture and ops reviewers, then a lead) is one example workflow that exercises it. Its roles and prompts are example content, kept out of the shipped product (Q2, re-scope).

What a workflow author gets:

```yaml
review_panel:
  kind: parallel
  branches:
    security_review:
      kind: agent_turn
      role: security            # a read_only role
      prompt_file: prompts/security.md
      capture: json
      results: [clean, blocking]
    architecture_review:
      kind: agent_turn
      role: architect
      prompt_file: prompts/architecture.md
      capture: json
      results: [clean, blocking]
  on: { done: lead_review }

lead_review:
  kind: agent_turn
  role: lead
  prompt_file: prompts/lead.md  # reads {{ stages.security_review.summary }} etc.
  capture: json
  on: { approved: open_pr, changes_requested: revising }
  loop_guard: { on: changes_requested, max: 3, then: escalate_to_human }
```

The rules, each decided in 01-idea.md:

| Rule | Decision |
|---|---|
| Shape | A `parallel` stage with its branches written inside it. Each branch is a uniquely named stage that reports a result but doesn't route (Q4) |
| Join | The group waits for every branch and then always leaves through `done`. Any decision belongs to a later stage (Q5) |
| Failure | If a branch fails, its siblings finish anyway. The task then parks `stuck`, naming each failed branch. Retry re-runs only the failed branches (Q6) |
| Worktree | No writers. Agent branches must use `read_only` roles. Shell and poll branches are allowed, under a group-wide check that the worktree is unchanged (Q7) |
| Limits | None. Every branch starts at once (Q8) |
| Re-entry | Every branch runs again each time the group is entered (Q9) |

## 2. Architecture

### 2.1 Where the state lives

`workflow_state.current_stage` names the **group** while it runs, and `stage_kind` is `"parallel"`. To everything outside the engine the task is "in `review_panel`", exactly as it is "in `internal_review`" today. The stage trail shows one row for it, and time-in-stage is the group's wall time.

Which branches are running, done or failed is kept in engine-owned payload, next to the existing engine-owned keys (`arrival`, `finished_stages`, `poll_window`):

```json
"parallel": {
  "stage": "review_panel",
  "entry": 2,
  "branches": {
    "security_review":     { "state": "running", "started_at": "…" },
    "architecture_review": { "state": "done", "result": "blocking", "ended_at": "…" },
    "tests":               { "state": "failed", "reason": "…", "resumable": true, "ended_at": "…" }
  }
}
```

- `entry` is the group's lap: the nth time the task entered it. Every branch's watcher carries the `entry` it was started under. A result from an earlier entry is dropped, never credited to a later one.
- **No migration is needed.** The payload is one JSON column, written under the per-task lock in the same UPDATE that records each fact (research §"What this settles"). `GET /tasks/{id}` already returns the payload. The API also adds a derived `workflow_state.branches` array, so the CLI doesn't parse engine internals.
- Branch captures go where every capture goes, `payload.stages.<branch>`. That is why `{{ stages.security_review.summary }}` works with no template change.

### 2.2 Lifecycle

**Entering the group.** Entry happens through `advance_from_stage`, as for any stage. Its single UPDATE already stamps the next stage's poll window, and in the same way it now also writes the `parallel` block, with every branch `running` and `entry` incremented. `enter_stage` then starts every branch, still under the task lock. Each branch's start is dispatched to the existing per-kind code (`enter_agent_turn`, and later `enter_shell` / `enter_poll`) with the branch's own name and definition. Each start records a `branch_started` event on the task timeline.

If one branch fails to start (a session won't spawn, a template is malformed, a role is rejected), only that branch is marked `failed`, and its siblings still start. If every branch fails to start, the group settles at once and parks.

**A branch finishes.** The watcher of an agent turn, shell or poll calls a new `finish_branch` instead of `advance_from_stage`. Under the task lock it:

1. stops if the task is cancelled, or if the group, its entry or the branch's `running` state no longer match (the same benign race `StageMovedOn` covers today);
2. checks the reported result against the branch's `results:`. A result outside the list makes the branch `failed`, the same way an outcome with no `on:` edge parks a single stage today;
3. merges the capture into `stages.<branch>`, adds the branch to `finished_stages` on success, and sets its state;
4. if siblings are still running, writes that one UPDATE and returns;
5. otherwise **settles the group** in the same write (below).

A branch whose turn can't complete (no report, a crash, a lingering process, a read-only violation, a usage limit) takes the same route through step 3 as `failed`, carrying the reason `park_incomplete_turn` builds today. The failure is also appended to the timeline at once, so it is visible before the group settles.

**Settling.** When the last branch is no longer `running`:
- **every branch is `done`** (and, from Phase 2, the group's worktree snapshot is unchanged): the same UPDATE sets `arrival = {from: group, outcome: done}`, marks the group finished, removes the `parallel` block and moves `current_stage` to the group's `done` target. Then `enter_stage` runs for that target. This is `advance_from_stage`'s transition, reached from the last branch.
- **any branch failed:** the payload update and the task's `stuck` status are written in **one SQLite transaction**. The `stuck` write is `mark_stuck`'s compare-and-set, which succeeds only if the task is still open. Today `mark_stuck` is a separate statement after the state write. Here the two are one fact, "this group settled with failures", so they commit together. The stuck reason names each failed branch and why.

**Cancel.** No change. `cancel_task` already kills every session the task ever had and aborts every detached runner. Branch watchers see `end_reason = cancelled` and return without writing. The `parallel` block stays as it was, and `choco task status` shows which branches were cut off.

**Retry** (`choco task retry` on a task stuck in a group):
- only branches in `failed` state are re-entered. Each one is resumed when its last session was cut off from outside (usage limit, reaper, daemon stop), and started fresh otherwise. The rules are the existing `resumable_session` rules, applied to each branch's own session;
- `done` branches keep their captures and are not paid for again;
- the re-entered branches go back to `running` in one UPDATE, written before `reopen_stuck`, the same order `retry_task_locked` uses today so a fast second failure is never lost. `entry` is unchanged, because a retry isn't an entry;
- `--resume` (strict) fails with `NotResumable` if any failed branch can't be resumed. `--fresh` starts them all fresh;
- `RetryOutcome` gains a per-branch list (additive). Its existing `stage` and `resumed` fields describe the group;
- a group that parked only because its worktree snapshot changed (Phase 2) has no failed branch. Its retry re-runs the settle step against the snapshot, so it passes once the operator has reset the worktree.

**Daemon restart.** The startup sweep handles a group task under its lock in one pass:
- each `running` agent branch is marked `daemon_stopped` on its session and becomes `failed` and resumable;
- each running shell branch becomes `failed`;
- each running poll branch (Phase 2) is resumed with the deadline stored on its branch entry, under the same ownership check the poll sweep uses today, applied per branch.

If nothing is left running, the group settles. Since a branch failed, that means it parks. This is the Q6 rule applied to a restart: once nothing is running the task is stuck, and retry resumes only what was cut off. `GET /server`'s `in_flight` lists a group task with the kinds of its still-running branches, so `choco update` warns as it does today.

### 2.3 Worktree access

- Every agent branch's role must be `read_only`, checked at load time. `read_only` already requires `worktree: true` and the three disallowed tools. Each agent branch keeps the per-turn baseline and check it has today.
- **Phase 2**, once a group has a shell or poll branch: the group takes its own worktree snapshot when it is entered, with the same snapshot function and the same exclusions (ignored files, `.git`), recorded as a task-scoped `worktree_baseline` event. It compares the snapshot when the group settles. A change parks the task, naming the branches that ran.
- A reader's own violation message lists the branches that ran beside it. A shell sibling that rewrote a tracked file then shows up as a suspect, and the reader isn't the only one blamed.
- Build output in ignored folders is outside both checks, so branches that build or test are fine.

### 2.4 Templates

- A later stage reads `{{ stages.<branch>.<field> }}`, unchanged.
- A branch's own prompt is rendered against the payload as committed when the group was entered. On a later lap it therefore sees its own previous capture, which is how today's reviewer reads its earlier report.
- A branch **may not reference a sibling** (a load-time error). The sibling is running concurrently, so the only value the branch could see is the sibling's report from the previous lap, which is misleading.
- `{{ stages.<group>… }}` is a load-time error because a group captures nothing. This falls out of today's `TemplateStageCapturesNothing` rule.

### 2.5 Loop guards and routing

Branches have no `on:` and no `loop_guard`. The group has exactly `on: { done: <stage> }` and no `loop_guard`. Lap limits belong on the stage that decides, such as the lead above. No `on:` target and no `loop_guard.then` may name a branch: a branch is entered only through its group.

### 2.6 Observability and cost

- **Timeline:** the group's `stage_entered` row, then `branch_started` and `branch_finished` events (result, or failure reason) per branch. Session events carry their session as today, so each branch's output stays separate.
- **`choco task status`:** under `Stage review_panel`, a branch table with columns branch, kind, state, result or reason, time, and cost. The progress table keeps one row per trail entry.
- **Dashboard:** the stage cell reads `review_panel 2/3` (settled/total). The detail view shows the branch table.
- **Cost:** usage is already grouped by session stage, role and lap. Each branch gets its own line with no usage change, apart from one SQL change: the session `lap` count includes `branch_started` events (retries excluded), so a branch's lap matches its group's entry.
- **Wall time:** a group's active time is one trail span, so it counts as the slowest branch, not the sum of the branches.

### 2.7 claude and omp

The mechanism lives entirely in the engine. Each branch is an ordinary session started through the adapter registry, so it works on both CLIs, and branches in one group may use different CLIs or models (for example, one branch on omp). Neither adapter writes per-task files that two concurrent sessions in one worktree could collide on (research, adapters row). Nothing is unsupported on omp.

One operational difference: N concurrent sessions on one vendor account reach its rate limit N times as fast. A usage-limit cut-off already parks as resumable, so retry resumes only the branches it hit. This design doesn't use sub-agents inside a turn (the owner's idea #1), so omp's lack of a `task` tool in choco doesn't matter.

### 2.8 Resources

There is no cap (Q8). A group of N branches runs N agent processes at once. Whatever each branch's prompt does (a scratch clone and build is 3–5 GB here) happens N times concurrently. `docs/workflows.md` says so plainly and tells authors to bound how many branches build. Concurrent branches can't safely share one scratch build, because they would race to create it. A daemon-wide cap would be a separate feature and is not part of this one.

## 3. Low-level details that matter

### 3.1 YAML and loader rules

- `kind: parallel` takes `branches:` (a map, at least two entries), plus `on:` with exactly one key, `done`.
- Each branch is a stage definition. In Phase 1 its kind must be `agent_turn`; in Phase 2 `shell` and `poll` are also allowed. `parallel`, `human_gate` and `terminal` are never branch kinds.
- A branch has **`results:`** in place of `on:`: the outcomes it may report, none of which route.
  - The key is `results`, not the `outcomes` shown in Q4's sketch, because `poll` already uses `outcomes:` for its pattern list.
  - `agent_turn`: defaults to `[done]`. With `capture: json` the agent picks from the list, and the `report_outcome` tool's allowed values are derived from it, as they are from `on:` today. Without `capture: json` it must be `[done]`.
  - `shell`: a subset of `{done, error}`, default `[done]`. A non-zero exit when `error` isn't listed fails the branch.
  - `poll`: a subset of its patterns' `then`s plus `timeout` and `error`. The default is its patterns' `then`s, so a timeout fails the branch unless listed.
- Branch names are unique across all stages and branches. No `on:` target, `loop_guard.then` or sibling template may name a branch.
- Every agent branch's role is `read_only`.
- `report_sections`, `capture` and `prompt_file` work on a branch exactly as on a stage.
- The loader keeps `definition.stages` as the top-level graph and adds a flattened lookup (branch name → group and definition). The engine resolves a branch through that lookup wherever it resolves a stage by name today. `reaches` and `sink_reachable_from_start` treat a group as one node with its `done` edge.

### 3.2 Events

- `branch_started`: task-scoped. Carries `group`, `branch`, `kind`, `entry`, and `via` (null, `retry` or `retry_resume`).
- `branch_finished`: task-scoped. Carries `group`, `branch`, `entry`, `state`, and `result` or `reason`.
- Both are new `EventType` values. Retention prunes them like any event. The session `lap` survives pruning because it is stored on the row, as today.

### 3.3 The one transaction

`settle_with_failures` runs `workflow_state::update` and `tasks::mark_stuck` (compare-and-set on `open`) in one `sqlx` transaction. Both need tx-accepting variants. If the task was no longer open (it was cancelled in between, which the lock should already make impossible), the transaction commits the payload and leaves the status alone. That is the same "the compare-and-set did its job" outcome `mark_stuck` logs today. The `Error` timeline event is appended after the commit, best-effort, as now.

### 3.4 What stays out

Nothing is added for:
- a per-branch rerun opt-out (Q9);
- early stop (Q5);
- writer branches in their own worktrees (Q7);
- a concurrency cap (Q8);
- nested groups.

Each of these can be added later as an optional key, without changing YAML written for this design.

## 4. Phases and how each is measured

**Phase 1: agent branches.** This phase covers:
- the loader rules (§3.1, `agent_turn` only);
- entry, `finish_branch` and settle;
- failure, retry, cancel and restart for agent branches;
- the per-branch lap count;
- events;
- the API's `branches`, `choco task status` and the dashboard;
- `docs/workflows.md`, and the `run-choco-task` skill (status output and group retry), in the same PR, per CLAUDE.md.

Phase 1 is measured as follows:
- **Deterministic engine tests** against `mock-claude`. Each is a named behaviour:
  - all branches done → `done`;
  - a result outside `results` → failed;
  - one failure while siblings finish, then park;
  - retry re-runs only the failed branch (asserted by session rows per branch);
  - retry resumes an interrupted branch;
  - cancel mid-group kills every branch session;
  - a restart mid-group parks with each running branch resumable;
  - a turn from an earlier entry is dropped;
  - re-entry runs every branch and increments `entry`;
  - `lap` per branch matches the group's entry;
  - usage `by_lap` has a line per branch;
  - the settle-with-failure write is atomic (inject a failure between the two statements and assert neither or both).
- **One live run on the real `claude`** (not mock) with the example panel (§6). Pass conditions:
  - the group's wall time is at most the slowest branch's plus 10%;
  - `choco task status` shows a cost line per branch;
  - one deliberately failed branch, then `choco task retry`, produces exactly one new session, for that branch only.
- The full gate, with the usual check that the suite count is at least 10.

**Phase 2: shell and poll branches.** This phase adds the extra branch kinds, the group worktree snapshot, a poll deadline per branch, and poll resume after a restart. It is measured as follows:
- **Tests:**
  - a poll branch survives a restart with its original deadline;
  - a shell branch that rewrites a tracked file parks the group, naming it, while a reader running alongside isn't the only one named;
  - a non-zero exit with `error` not in `results` fails the branch.
- **One live run** with a test-running shell branch beside an agent branch.

**Later, only if a use case asks for it.** A rerun opt-out, early stop, writer branches in their own worktrees, a daemon-wide cap. None is planned.

**The example panel is measured separately**, after Phase 1. It decides Q2's promote-or-delete:
- run it on five or more real tasks alongside the plain `coding-task`;
- record, per task:
  - blocking findings that only a specialist branch raised;
  - cost per review lap;
  - wall time per lap.
- the owner decides on those numbers. The mechanism ships whatever the example's result.

## 5. Compatibility and upgrade

- **Existing workflows are untouched.** `parallel` is a new kind, and no existing key changes meaning. Built-ins don't use it.
- **An older daemon** rejects a workflow containing `kind: parallel` at load time, as an unknown kind. So `--workflow` with the example fails at `task create` on an old daemon, not mid-task.
- **Downgrade with a task sitting in a group:** the old daemon can't load that task's workflow, so the startup sweep parks it with "could not load this task's workflow". Finish or cancel group tasks before downgrading. The release notes say so.
- **API and CLI.** `workflow_state.branches` and the per-branch `RetryOutcome` list are additive. An older `choco` shows the group name as the stage and ignores the rest.
- **DB:** no migration. One SQL change (the `lap` count), compatible with every existing row.
- **MCP:** no change. The tool's allowed values come from the branch's `results:` the way they come from `on:` today.

## 6. The example workflow

Per Q2, `workflows/experimental/review-panel.yaml` plus its prompts:
- it lives in the repo but is **not embedded** in the daemon, so `init-workflows` doesn't seed it;
- a test loads every file under `workflows/experimental/` so it can't rot;
- its prompts are generic example content (security, architecture and operational readiness readers, and a lead that decides). Anything specific to how choco itself is developed stays in our uncommitted role-prompt overlay, never in these files.

**Running it: a pinned checkout, not hashing.** A `--workflow` task reads its prompts and scripts from the checkout at run time, and edits made mid-task aren't detected. #162 proposed hashing them and was closed as not planned. So every measured panel run is created from a checkout nobody changes while the task runs: a detached worktree at a fixed commit, created for the measurement and left alone until the task closes. `ChocoFactory-base` is not used, because it moves with `main`. This needs no code, and it fits the decision already taken on #162.

## 7. Risks

- **Rate limits** are reached N times as fast on one account. This is mitigated by resumable usage-limit interruptions and retry of only the cut-off branches. It is still the most likely cause of a parked group in practice.
- **Disk.** The engine adds no cap, so N branches whose prompts build are N builds at once. This is documented. The example should let at most one branch build (the others read and reason), and its measured runs should check `df -h` first, as every wave does today.
- **Lock contention** is negligible: branch completions are minutes apart, and each holds the task lock for a few queries.
- **Payload growth:** one small block per group, removed when the group settles.
