# 04 — Implementation plan: parallel stage groups

Based on `03-design.md`, approved by the owner on 2026-10-09. Tracking issue #257.

Tasks follow the design's phases (§4). Each task names its design sections, its dependencies, the files it mainly touches, and the tests that mark it done. Paths are relative to the repo root and were read at `main` `9ea2678`.

**Applies to every task:**
- The full gate from `CLAUDE.md`: build all targets first, then test, fmt and clippy. A suite count below 10 means a short run.
- The self-check for this repo's two recurring review findings. One write per fact, so no read-then-write on task or run status. No swallowed `Result`.
- Branch state is engine-owned payload (design §2.1). Write it only under the per-task lock, in the same UPDATE as the fact it records.
- No migration. If a task seems to need one, stop and raise it, because the design says none is needed.

**Waves** (at most 3 tasks per wave):

| Wave | Tasks |
|---|---|
| A | PG1-1, PG1-2, PG1-3 (independent, touch different files) |
| B | PG1-4 |
| C | PG1-5, PG1-6, PG1-7 |
| D | PG1-8, then PG1-9 (a measured run, not code) |
| After Phase 1 | M-1 (the example's measurement), Phase 2 when wanted |

Don't cut a release between Wave B and the end of Wave D. Before PG1-8, `kind: parallel` is accepted but undocumented, and retry and status don't understand it yet.

## Phase 1 — agent branches

### PG1-1. Loader: `kind: parallel`, branches and `results:`

Teach the workflow loader the new kind. This task only parses and validates. The engine doesn't run a group yet.

- A `StageKind::Parallel` stage takes `branches:`, a map with at least two entries, and `on:` with exactly the one key `done`.
- Each branch is a `StageDef`. In this phase its kind must be `agent_turn`. `parallel`, `human_gate` and `terminal` are never allowed as branch kinds. `shell` and `poll` are rejected with an error saying they come in a later version (PG2-1 lifts this).
- A branch has `results:` (a list of outcome names) instead of `on:`. For `agent_turn` it defaults to `[done]`. With `capture: json` any list is allowed. Without it, the list must be `[done]`.
- Errors, each with its own `WorkflowDefError` variant and message:
  - a branch with `on:` or `loop_guard`;
  - a group with `loop_guard`, or `on:` keys other than `done`;
  - a branch name used twice across all stages and branches;
  - an `on:` target or `loop_guard.then` naming a branch;
  - a branch whose role isn't `read_only`;
  - a branch prompt that references a sibling (`{{ stages.<sibling>… }}`);
  - any template that references a group (`{{ stages.<group>… }}`). This should come from the existing `TemplateStageCapturesNothing` rule.
- Add a flattened lookup (branch name → group name and `StageDef`), alongside `definition.stages`, which stays the top-level graph. `reaches` and `sink_reachable_from_start` treat a group as one node with its `done` edge.
- `report_sections`, `capture` and `prompt_file` work on a branch as on a stage. Prompt files of branches are loaded like stage prompt files.

Files: `chocofactoryd/src/workflow_def.rs` (`StageDef`, `StageKind`, `validate`, `validate_templates`, `reaches`, `sink_reachable_from_start`).

Tests: one loader test per rule and error above, plus one valid two-branch group whose later stage reads `{{ stages.<branch>.summary }}`.

- Design ref: §3.1, §2.4, §2.5, §2.3 (first bullet)
- Depends on: none

### PG1-2. Events and the per-branch lap

- Add `EventType::BranchStarted` and `EventType::BranchFinished` (task-scoped), with the payloads in design §3.2:
  - `branch_started`: `group`, `branch`, `kind`, `entry`, `via` (null, `retry` or `retry_resume`);
  - `branch_finished`: `group`, `branch`, `entry`, `state`, and `result` or `reason`.
- Change the session `lap` SQL so a session's lap also counts `branch_started` events whose `branch` is the session's stage. Retries are excluded, as `stage_entered` retries are today. Existing rows and non-branch stages must compute exactly as before.

Files: `chocofactory-core/src/models.rs` (`EventType`), `chocofactoryd/src/db/sessions.rs` (`create_inner`), and the event serialisation tests.

Tests:
- `lap` for a branch session is 1, 2, 3 across three `branch_started` events, and doesn't advance on a `via: retry` event;
- `lap` for an ordinary stage is unchanged when `branch_started` events for other names exist;
- both events round-trip through the API's event serialisation.

- Design ref: §3.2, §2.6 (Cost)
- Depends on: none

### PG1-3. Transaction-capable state and stuck writes

Add a `settle_with_failures` write that commits the `workflow_state` payload update and the task's `stuck` status in one `sqlx` transaction.

- Add transaction-accepting variants of `workflow_state::update` and `tasks::mark_stuck`. Keep the existing pool versions as thin wrappers, so no caller changes.
- `mark_stuck` keeps its compare-and-set on `open`. If the task is no longer open, the transaction still commits the payload, leaves the status alone and reports "not marked". This is the same outcome `mark_stuck`'s `false` means today. It must not be turned into an error, and must not be dropped silently either: return it to the caller.
- The `Error` timeline event is appended by the caller after the commit, best-effort, as today. It isn't part of this task.

Files: `chocofactoryd/src/db/workflow_state.rs`, `chocofactoryd/src/db/tasks.rs`.

Tests:
- both writes land together;
- a forced failure on the second statement leaves neither (roll back the first);
- the task not being `open` commits the payload, keeps the status and returns "not marked".

- Design ref: §3.3, §2.2 (Settling)
- Depends on: none

### PG1-4. Engine: enter a group, finish branches, settle

The core of the feature, for `agent_turn` branches.

- **Entry** (design §2.2, "Entering the group"):
  - `advance_from_stage`'s single UPDATE writes the `parallel` block when the target is a group. Every branch is `running` with `started_at`, and `entry` is the previous entry plus 1 (first entry is 1). `current_stage` is the group and `stage_kind` is `"parallel"`.
  - `enter_stage` starts each branch through `enter_agent_turn`, with the branch's own name and `StageDef`, still under the task lock. Each start records `branch_started`.
  - A branch that fails to start becomes `failed`, and its siblings still start. If every branch fails to start, the group settles at once.
  - A branch prompt is rendered against the payload as committed at entry.
  - `report_outcome`'s allowed values for a branch come from its `results:`, where they come from `on:` today (`engine/turn.rs`, the `stage_def.on.keys()` site).
- **`finish_branch`** (design §2.2, "A branch finishes", steps 1–5). The agent turn's watcher calls it instead of `advance_from_stage` when the stage is a branch. Under the task lock:
  1. return without writing if the task is cancelled, or if the group, the `entry` the watcher was started under, or the branch's `running` state no longer match;
  2. a result outside `results:` makes the branch `failed`;
  3. merge the capture into `stages.<branch>`, add the branch to `finished_stages` on success, set the branch state and `ended_at`;
  4. if siblings are still running, that one UPDATE is the whole write;
  5. otherwise settle the group in the same write.

  Record `branch_finished` for each branch that settles.
- **Failed turns.** Every path that today goes through `park_incomplete_turn` → `mark_stuck` instead marks the branch `failed` when the stage is a branch: no report, crash, lingering process, read-only violation, usage limit. The branch keeps the reason text it would have had and a `resumable` flag from the existing `resumable_session` rules. The failure is appended to the timeline at once.
- **Settle:**
  - all branches `done`: one UPDATE sets `arrival = {from: <group>, outcome: done}`, adds the group to `finished_stages`, removes the `parallel` block and moves `current_stage` to the group's `done` target. Then `enter_stage` runs for that target;
  - any branch `failed`: `settle_with_failures` (PG1-3). The stuck reason names each failed branch and its reason.
- **Read-only.** A reader's violation message lists the sibling branches that ran beside it (design §2.3, third bullet).
- **Cancel** needs no code. Prove it with the test below.

Files: `chocofactoryd/src/engine/mod.rs` (`advance_from_stage`, `enter_stage`, `dispatch_stage`), `chocofactoryd/src/engine/turn.rs` (`enter_agent_turn`, `finish_turn`, `park_incomplete_turn`, `is_current_run_for_stage`), and a new `chocofactoryd/src/engine/parallel.rs` if `mod.rs` gets crowded.

Tests (design §4, Phase 1, against `mock-claude`), one each:
- all branches done → the task moves to the `done` target, and the next stage's prompt renders both branches' summaries;
- a result outside `results` → that branch is failed;
- one branch fails while its sibling finishes, then the task parks `stuck` with a reason naming the failed branch;
- a finished turn from an earlier entry is dropped and changes nothing;
- re-entering the group (through a later stage routing back to it) runs every branch again and increments `entry`;
- cancel mid-group kills every branch session, and no branch writes afterwards;
- `lap` per branch session matches the group's entry;
- usage `by_lap` has a line per branch;
- a branch that fails to start doesn't stop its sibling.

- Design ref: §2.1, §2.2 (entry, finish, settle, cancel), §2.3, §2.4
- Depends on: PG1-1, PG1-2, PG1-3

### PG1-5. Retry of failed branches

`choco task retry` on a task stuck in a group.

- Re-enter only branches in `failed` state. Resume each one whose last session was cut off from outside (`resumable_session` per branch session). Otherwise start it fresh. `done` branches are left alone, with their captures.
- The re-entered branches go back to `running` in one UPDATE, written before `reopen_stuck`. This is the order `retry_task_locked` uses today. `entry` doesn't change. Each start records `branch_started` with `via: retry` or `retry_resume`.
- `--resume` fails with `NotResumable` if any failed branch can't be resumed, and changes nothing. `--fresh` starts every failed branch fresh.
- `RetryOutcome` gains an additive per-branch list: branch, resumed, `adapter_session_id`, and the reason it started fresh. `stage` names the group and `resumed` is true only if every re-entered branch resumed. `choco task retry` prints the list.
- Update the `run-choco-task` skill's retry guidance in the same PR (`CLAUDE.md` rule).

Files: `chocofactoryd/src/engine/mod.rs` (`retry_task_locked`, `resumable_session`), `chocofactory-core/src/models.rs` (`RetryOutcome`), the `choco` retry command's output, `.claude/skills/run-choco-task/`.

Tests:
- retry re-runs only the failed branch, asserted by a new session row for that branch and none for the others;
- retry resumes a branch that was interrupted by a usage limit;
- `--resume` with one non-resumable failed branch is refused, and nothing changes;
- a second failure that comes in fast during retry still parks (the order of writes).

- Design ref: §2.2 (Retry), §5 (API)
- Depends on: PG1-4

### PG1-6. Daemon restart, `in_flight` and messages

- **Startup sweep.** In one pass under the task's lock: each `running` agent branch's session is marked `daemon_stopped`, and the branch becomes `failed`, resumable. When nothing is left running, the group settles, so it parks (PG1-3's write).
- **Ownership checks.** `has_detached_runner` and the restart sweep's ownership check ask per branch for a group task.
- **`in_flight`** (`GET /server`) lists a group task with the kinds of its still-running branches, so `choco update` warns as it does for a single stage.
- **`send_message` / `send_message_or_resume`** refuse a task whose current stage is a group, with the same error a single-shot stage gives today.

Files: `chocofactoryd/src/engine/sweep.rs` (`restart_effect`, `park_interrupted_turn_locked`, `in_flight`), `chocofactoryd/src/engine/runners.rs`, `chocofactoryd/src/engine/mod.rs` (`send_message*`).

Tests:
- a restart mid-group parks the task, with each running branch `failed`, resumable, and its session `daemon_stopped`;
- a following retry resumes exactly those branches;
- `in_flight` lists a running group with its branch kinds;
- `choco task send` to a task in a group is refused.

- Design ref: §2.2 (Daemon restart); `02-research-single-stage-assumptions.md`, rows `in_flight` and `send_message`
- Depends on: PG1-4

### PG1-7. API `branches`, `choco task status` and the dashboard

- **API.** `GET /tasks/{id}` gains a derived `workflow_state.branches` array, built from the `parallel` block: name, kind, state, result or reason, started and ended times. The array is empty when no group is current. It's additive, so an older `choco` ignores it.
- **`choco task status`.** Under `Stage <group>`, a branch table with the columns branch, kind, state, result or reason, time, and cost (cost from the usage aggregation by stage and lap). The progress table keeps one row per trail entry.
- **Dashboard.** The stage cell reads `<group> settled/total`, for example `review_panel 2/3`. The detail view shows the branch table.
- Update the `run-choco-task` skill's status-reading guidance in the same PR.

Files: `chocofactory-core/src/models.rs` (`WorkflowState`), `chocofactoryd/src/api/tasks.rs`, `choco/src/render.rs` (`detail_stage`, `detail_progress`), `choco/src/dashboard/view.rs`, `.claude/skills/run-choco-task/`.

Tests:
- render tests for the branch table (running, done, failed rows);
- the dashboard cell renders `2/3`;
- the API returns `branches` for a task in a group and `[]` otherwise;
- an older-shape response (no `branches`) still renders.

- Design ref: §2.6, §5 (API and CLI)
- Depends on: PG1-4 (the payload shape)

### PG1-8. The example workflow and the docs

- **`workflows/experimental/review-panel.yaml`** with its prompts: three read-only reader branches (security, architecture, operational readiness) and a `lead_review` stage that decides, shaped like design §1. The prompts are generic example content, with nothing about how ChocoFactory itself is developed. At most one branch may build (design §7, Disk). Not embedded: don't add it to `config_root.rs`'s built-in lists.
- **A test** that loads every YAML file under `workflows/experimental/` through the loader, so the example can't rot.
- **`docs/workflows.md`**: a section on `kind: parallel`. Cover the YAML shape, `results:`, the join and failure rules, retry, the read-only requirement and the template rules. State the resource cost plainly: N branches are N processes, and N builds if their prompts build. Tell authors to bound how many branches build, and say concurrent branches can't share one scratch build.
- **Release notes text** for the compatibility points in design §5. An older daemon rejects the kind at `task create`. Finish or cancel any task in a group before downgrading.

Files: `workflows/experimental/`, a new loader test, `docs/workflows.md`.

- Design ref: §6, §2.8, §5, §7
- Depends on: PG1-1 (loader). Write the docs after PG1-5 and PG1-7 merge, so they describe the real retry and status output.

### PG1-9. Phase 1 live run (measurement, not code)

One run of the example panel on the real `claude`, with `CHOCOFACTORY_CLAUDE_BINARY` unset. Create it from a detached worktree pinned at the merge commit, left unchanged until the task closes (design §6). Run `df -h` first.

Pass conditions (design §4):
- the group's wall time is at most the slowest branch's plus 10%;
- `choco task status` shows a cost line per branch;
- one branch deliberately failed (for example, a prompt that reports a result outside its `results:`), then `choco task retry`, gives exactly one new session, for that branch only.

Record the numbers on #257.

- Design ref: §4 (Phase 1 measurement), §6
- Depends on: PG1-1 to PG1-8

## The example's measurement

### M-1. Promote-or-delete run of the review panel

Run the example panel on five or more real tasks, alongside the plain `coding-task`. Use the pinned checkout from PG1-9 and run `df -h` before each run. Record per task:
- blocking findings that only a specialist branch raised;
- cost per review lap;
- wall time per lap.

The owner decides whether to promote or delete it on those numbers (Q2). The mechanism ships whatever the result.

- Design ref: §4 (the example panel), §6
- Depends on: PG1-9

## Phase 2 — shell and poll branches

Start only when a use case asks for it. The tasks are listed so the Phase 1 code leaves room for them.

### PG2-1. Loader: shell and poll branch kinds

- Allow `shell` and `poll` as branch kinds. Lift PG1-1's "later version" error.
- `results:` for `shell` is a subset of `{done, error}`, default `[done]`.
- `results:` for `poll` is a subset of its patterns' `then`s plus `timeout` and `error`. The default is its patterns' `then`s.

Tests: one per rule.

- Design ref: §3.1
- Depends on: Phase 1

### PG2-2. Engine: shell and poll branches

- Start branches through `enter_shell` and `enter_poll`, and finish them through `finish_branch`.
- A non-zero shell exit with `error` not in `results` fails the branch. A poll timeout not in `results` fails the branch.
- **Poll deadline per branch.** Each poll branch's deadline is stored on its branch entry in the `parallel` block, not in the single `payload.poll_window` slot.
- `still_in_stage` for a poll branch checks the group, the entry and the branch's `running` state.

Files: `chocofactoryd/src/engine/watch.rs` (`set_poll_window`, `still_in_stage`), the shell runner, `chocofactoryd/src/engine/parallel.rs`.

Tests:
- a non-zero exit with `error` not in `results` fails the branch;
- a poll branch times out on its own deadline;
- a shell branch's capture is readable by the next stage.

- Design ref: §2.2, §3.1
- Depends on: PG2-1

### PG2-3. Group worktree snapshot

- A group that has a shell or poll branch takes a worktree snapshot when it is entered. It uses the existing snapshot function and exclusions, recorded as a task-scoped `worktree_baseline` event.
- At settle, a changed worktree parks the task, naming the branches that ran, even if every branch is `done`.
- Retry of a group parked only for this reason re-runs the settle check.

Tests:
- a shell branch that rewrites a tracked file parks the group, naming it, and a reader running alongside isn't the only one named;
- retry after the operator resets the worktree passes the check and moves on.

- Design ref: §2.3, §2.2 (Retry, last bullet)
- Depends on: PG2-2

### PG2-4. Restart for shell and poll branches

- At startup, a `running` shell branch becomes `failed`.
- A `running` poll branch is resumed with the deadline stored on its branch entry, under the poll sweep's ownership check applied per branch.
- `in_flight` reports these kinds.

Tests:
- a poll branch survives a restart with its original deadline;
- a shell branch is failed by a restart and re-run by retry.

- Design ref: §2.2 (Daemon restart)
- Depends on: PG2-2

### PG2-5. Phase 2 docs and live run

- Update `docs/workflows.md` and the `run-choco-task` skill for shell and poll branches.
- Do one live run on the real `claude` with a test-running shell branch beside an agent branch, from a pinned checkout, running `df -h` first.

- Design ref: §4 (Phase 2 measurement)
- Depends on: PG2-3, PG2-4
