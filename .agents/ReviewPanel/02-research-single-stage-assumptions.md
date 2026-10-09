# Research: where the engine assumes one running stage

The question this answers: if a task can run several stages at once (a parallel group, as decided in 01-idea.md Q4–Q9), which parts of today's engine break, which keep working unchanged, and what does each need?

Read from `main` at `a1894df` (2026-10-09). Paths are relative to `chocofactoryd/src/` unless they name another crate.

## The short answer

Most of the engine is keyed by **stage name**, not by "the current stage". That includes:
- captures and templates;
- sessions;
- usage, including per-lap usage;
- the read-only baseline;
- session resume.

All of these work for branches unchanged, provided each branch is a uniquely named stage (Q4).

The single-stage assumption lives in a small number of places, all of which read `workflow_state.current_stage` and act on that one stage:
- the advance path;
- the watchers' "still in my stage?" checks;
- `mark_stuck`;
- retry;
- the restart sweeps;
- `in_flight`;
- the one `poll_window` slot;
- the CLI's status and dashboard.

None of them needs a schema change. Each needs a group-aware branch next to its single-stage path.

## Inventory

### Works unchanged when every branch is its own named stage

| Site | Today | With a group |
|---|---|---|
| Captures: `merge_stage_capture` → `payload.stages.<stage>` (`engine/stage_capture.rs`) | keyed by the stage that produced it | each branch writes `stages.<branch>`. Later stages read `{{ stages.<branch>.summary }}` with today's syntax |
| `template::render` and `finished_stages` (`template.rs`) | "never ran" vs "ran, no capture" by stage name | the same, with branch names |
| Sessions (`db/sessions.rs`): one row per attempt, `stage` column | many rows per task are already normal | one row per branch attempt |
| `report_outcome` (`choco/src/mcp.rs`, `db/events.rs::last_report_outcome_for_session`) | read back **per session** | concurrent sessions can't see each other's reports. Nothing changes |
| Usage (`usage.rs::aggregate`, `db/usage.rs`): by stage, role, `(stage, lap)` | grouped by the session's stage, role and lap | a cost line per branch for free. Active time comes from the stage trail, so a group counts its wall time once, not the sum of its branches |
| Read-only baseline (`engine/turn.rs::read_only_baseline`, `read_only_verdict`) | per session: a `worktree_baseline` event on the session, compared when its turn ends | per branch session, unchanged. See "Subtle" below for siblings |
| Resume (`resumable_session`, `sessions::resume_chain_len`) | decided from one session's end reason | decided per branch session |
| Cancel (`cancel_task_locked`) | kills **every** session the task ever had, then aborts **every** detached runner of the task | already covers N branches. Turn watchers see `end_reason = cancelled` and return; runners are aborted |
| Graceful shutdown and the idle reaper (`session.rs`) | per session | per session |
| Adapters (`adapter/claude.rs`, `adapter/omp.rs`) | claude: `--mcp-config` is inline JSON, nothing is written per task. omp: overlay file per UUID; sessions in a shared `--session-dir`, one file per session | two concurrent sessions in one worktree don't collide on either CLI |
| Concurrency limits | none exist, per task or daemon-wide | none added (Q8) |

### Assumes one current stage: needs a group-aware path

| Site | What it assumes | What a group needs |
|---|---|---|
| `advance_from_stage` (`engine/mod.rs`) | the finishing stage **is** `current_stage`, and its outcome picks an `on:` edge. One UPDATE writes stage, counters, capture, arrival, `finished_stages` and poll window | a separate `finish_branch` path under the same per-task lock. It records the branch's result in one UPDATE. Only the last branch to settle moves `current_stage` on, through the group's `done` edge, in that same UPDATE |
| `expected_stage` / `StageMovedOn` | a detached runner's result applies only while its stage is current | a branch's result applies only while `current_stage` is its group, the group's **entry number** matches, and the branch is still `running`. Otherwise a finished turn from an earlier entry could be credited to a later one |
| `is_current_run_for_stage` (`engine/turn.rs`) | newest session of the stage | the same by branch name. The UUID tie-break caveat in its doc comment still applies only to two runs of one branch, which can't overlap |
| `park_incomplete_turn` → `mark_stuck` | a failed turn parks the **task** at once | a failed branch is recorded as failed and siblings keep running (Q6). The task parks only once every branch has settled |
| `mark_stuck` reason, `stage_to_blame` | one stage named | the reason names each failed branch and why |
| `retry_task_locked` | re-enters `current_stage` once, resuming its last session if interrupted | a group re-enters only its failed branches, each resumed or started fresh by its own session (Q6). `RetryOutcome` reports one stage and one `resumed` flag; it needs a per-branch list (additive) |
| `poll_window` (`engine/watch.rs::set_poll_window`, `poll_window_for`) | **one** `payload.poll_window` slot, stamped for `current_stage` | one deadline per poll branch, kept with that branch's state |
| `still_in_stage` (`engine/watch.rs`) | a poll keeps going while `current_stage == stage` | a poll branch keeps going while its group is current with the same entry and the branch is `running` |
| `has_detached_runner` / `abort_detached_runners` (`engine/runners.rs`) | "a runner exists for the task" means the current stage's watcher is alive | for a group, "some branch's runner is alive". The restart sweep's ownership check must ask per branch. Abort-all stays right for cancel, and for a gate reply (a gate is never a branch) |
| Restart: `restart_effect`, `park_interrupted_turn_locked`, `resume_interrupted_polls` (`engine/sweep.rs`) | classify the one current stage. An agent or shell stage is parked; a poll is resumed | classify each running branch. Agent and shell branches become failed (interrupted; an agent branch stays resumable). Poll branches resume with their deadline. The task parks once nothing is left running (Q6) |
| `in_flight` (`GET /server`) | one stage per open task | list the group, with the kinds of its still-running branches |
| `send_message` / `send_message_or_resume` | the current stage is a standing turn or a gate | a group is neither, so a message is refused, as for any single-shot stage today |
| `sessions.lap` SQL (`db/sessions.rs::create_inner`) | counts `stage_entered` events for the session's stage name | branches get no `stage_entered` (it would put each branch in the stage trail as if the task had moved there). The count must also include a new `branch_started` event, excluding retries, as now |
| `stage_entered_at`, `stage_kind` (`db/workflow_state.rs`) | the current stage's entry time and kind | the group's. `stage_kind = "parallel"`. `--until attention` keys on `human_gate`, so it is unaffected |
| Loop guards (`bump_loop_counter`, `clear_guards_escaping_to`) | keyed by the guarded stage; reset on arrival at `then:` | branches don't route, and a group only reports `done` (Q5), so neither carries a guard. The loader rejects one on either. `then:` may never name a branch |
| Loader (`workflow_def.rs::validate`, `validate_templates`, `reaches`, `sink_reachable_from_start`) | `stages` is flat; every `on:` target, template reference and `then:` resolves in it | branches are nested, so name uniqueness, template lookup and "nothing routes into a branch" need the flattened view |
| CLI status (`choco/src/render.rs::detail_stage`, `detail_progress`) and dashboard (`choco/src/dashboard/view.rs`) | one stage label and one "time in stage" | the group shows as one trail row and one current stage. The detail adds a branch table (state, outcome or reason, time, cost); the dashboard's stage cell shows progress (`review_panel 2/3`) |

### Subtle: the read-only check and siblings

Each read-only turn's check compares the **whole worktree** before and after the turn (#172). With read-only agent branches only, a sibling can't legitimately change anything, so the check still points at the right culprit. A shell or poll branch is a command nothing makes read-only. If one rewrites a tracked file, every reader running alongside fails its own check and gets parked, though it did nothing. Q7's answer handles this. The group takes its own snapshot at entry and compares it when it settles, and a reader's violation message names the branches that were running alongside it, so the operator isn't sent after an innocent reader.

### Not affected

- `human_gate` and watchers, because a gate can't be a branch.
- Terminal entry and branch cleanup (#102), which happen after the group.
- `choco project init-workflows`, because the example is not embedded (Q2).
- The MCP tool schema, which is per session and derived from the branch's own declared outcomes.

## What this settles for the design

1. **No migration is needed.** Branch state fits in engine-owned `workflow_state.payload`, the same way `poll_window`, `arrival` and `finished_stages` already do. It is written in the one UPDATE a transition already makes, under the per-task lock.
2. **The per-task lock is enough to serialize branch completions.** They are rare (minutes apart) and each holds the lock for a few queries.
3. **There is one new write pairing to make atomic.** When the last branch settles with a failure, the branch result and the task's `stuck` status are a single fact. Today `mark_stuck` is a second statement after the state write. The design puts the two in one SQLite transaction.
4. **Nothing in either adapter has to change** for claude/omp parity. One operational difference: N concurrent sessions reach a vendor's rate limit N times as fast. A usage-limit interruption already parks as resumable, so under Q6 only the interrupted branches re-run.
