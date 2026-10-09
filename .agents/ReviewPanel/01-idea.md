# Idea honing: review panel

A raw running log of questions and answers. Don't refactor it.

## Q1 (owner): sub-agents inside one reviewer (#1), or parallel stages in the workflow spec (#2)?

**pm's answer (2026-10-09), as a starting point for the architect. It isn't a decision.**

**Where the engine is today.** A task has exactly one current stage (`workflow_state.current_stage`). These are all built around that single stage:
- the per-task lock;
- loop guards;
- retry and cancel;
- the "stuck" state;
- cost per lap;
- restart recovery;
- the dashboard.

Stages already read earlier stages' reports through templates, for example `{{ stages.internal_review.summary }}` in `open_pr`.

**#1, sub-agents inside one reviewer.**
- **Harness-dependent:** omp may not have sub-agents.
- **Not enforceable:** nothing can make the reviewer actually start the panel.
- **Not observable:** there are no separate events, no cost per perspective, and no way to tell whether the security reviewer ran.
- **Same model for everyone:** each perspective inherits the main reviewer's model and CLI, so the security reviewer can't run on a different model.
- **Verdict:** cheap to try, but not a feature that can be enforced or measured.

**#2, parallel stages.** This is the real feature, and it touches every one-stage assumption above. A constrained form looks tractable: a **parallel group of read-only `agent_turn` branches that join** before the next stage.
- **Read-only is the key constraint:** concurrent readers of one worktree are safe. Concurrent writers would need their own worktrees and a merge step.
- **Rules a spec must settle:**
  - one stuck branch makes the group stuck, and a retry re-runs only that branch;
  - cancel kills every branch;
  - each branch has its own loop-guard count and cost line;
  - after a daemon restart, finished branches keep their reports and only unfinished ones re-run;
  - how the dashboard and `task status` show N active branches;
  - how a later stage's template addresses each branch's report.
- **Resource cost:** N reviewers building scratch copies at once costs N× the build disk and CPU. On the dev machine, 4 tasks plus 4 reviewers already filled the disk once.

**A step that works today with no engine change: a sequential panel.**

```
coding → security_review → architecture_review → ops_review → lead_review → open_pr
```

- **Specialists:** each is a normal `agent_turn` with its own prompt, and optionally its own model or CLI (for example, omp for one of them). Each always reports and moves on.
- **The lead:** its prompt includes `{{ stages.security_review.summary }}` and the others, and its verdict routes the task the way `internal_review` does today.
- **For:** enforced, works on any harness, cost and events per perspective, and read-only enforcement still applies.
- **Against:** latency is the sum of the reviews (about 4 × 5 minutes), not the longest one. Paid cost is the same as running them in parallel.

**The order pm suggests:**
1. Build the sequential panel as a workflow, plus specialist prompts.
2. Measure it on real tasks against a single reviewer: do the specialists catch what one reviewer misses? There's an early hint that they do. When #227 was split across two independent reviewers, one on code and one on safety, their blocking findings didn't overlap.
3. If the panel earns its cost and latency hurts, build #2 as "run these read-only stages concurrently". By then the prompts, the report format and the lead's merge logic are proven.

**Open questions for the architect:**
- Built-in workflow or documented example? The owner's rule is not to hard-code choco's own development process into what customers get. pm leans towards a documented example first.
- How does the lead treat a specialist's blocking finding? Can it overrule one, and must it say why?
- Does each specialist see the others' reports (a sequential chain), or only the diff (independent reviews)? Independent reviews are closer to what parallel would give, and avoid anchoring.
- Loop guards and revise laps: after `changes_requested`, does the whole panel re-run, or only the specialists whose findings were blocking?
- Should the parallel group's join support "first blocking finding wins", or always wait for all branches?

## Q2 (arch-panel): where does the panel live while it's unproven?

Context from the code: a sequential panel needs no engine change. Each specialist is an `agent_turn` with `capture: json` and an `on:` map like `{ clean: next, blocking: next }`; its report lands in `stages.<name>.summary`, its session row and usage are already kept per stage, and the lead reads them through templates. The specialist prompts themselves (security, architecture, operational readiness) are generic, not choco-specific; anything specific to how we build choco goes in our uncommitted role-prompt overlay, as today.

Options:
- **A. Documented example only.** A YAML and prompts shown in `docs/`, nothing embedded. For: no product surface. Against: nothing loads or tests it, so it rots; we'd dogfood from a path anyway.
- **B. A new built-in (`coding-task-panel`).** Embedded, seeded by `init-workflows`, tested. For: customers get it; it's versioned with the daemon. Against: four new prompts become product surface before we know they earn their cost, and a customer can pick a ~4× reviewer bill without the data to justify it.
- **C. Experimental, in the repo but not embedded** (e.g. `workflows/experimental/coding-task-panel.yaml` plus prompts), loaded by a test so it can't rot, dogfooded with `--workflow`. Promoted to a built-in (B) only if the measurement passes; deleted if it fails.

Recommendation: C.

**Owner's answer (2026-10-09): C.** Experimental, in the repo under `workflows/experimental/`, loaded by a test, not embedded in the daemon; dogfooded with `--workflow`; promoted or deleted on the measurement.

**pm's constraint, added with the answer:** a `--workflow` task reads its prompts and scripts from the checkout at run time, and edits made mid-task aren't detected (closed issue #162). Panel runs must either use a checkout nobody changes during the task (for example a pinned release checkout), or the design must hash the referenced files. 03-design.md says which.

## Q3 (arch-panel): who covers general correctness, and what is the lead?

Context: the owner's three specialists are security, architecture and operational readiness. None of them covers what today's reviewer is best at: correctness, the branch-to-test walk, old behaviour, and breaking the fix in a scratch clone (the walks in `reviewer-system.md`). The lead's job decides the cost of the lap and whether a panel run can be measured against a single reviewer.

Options:
- **A. The lead is today's full reviewer, plus judge.** It does every walk it does now, then folds the specialists' reports into one verdict. For: the panel is a strict superset of today's review. Each run measures itself: findings only a specialist raised are what the panel added. Against: the lead lap is the most expensive (it builds and experiments), and it reads the specialists' reports in the same turn, so they can anchor its own walk.
- **B. The lead only judges; a fourth specialist covers correctness.** For: symmetric, and a judge-only lead is cheap (no build). Against: five agents per lap, and a judge that doesn't read code can't settle a disputed finding.
- **C. The lead judges and may verify.** No full walks, but it reads code or runs a check to settle a disputed or conflicting finding. Correctness falls to a fourth specialist as in B. Middle cost, same five agents.

Recommendation: A.

**Owner's answer (2026-10-09), and a re-scope:**

> focus not on the particular use case but on the mechanism. Right now we start one workflow stage at a time; with this change we support running several at once. The main reviewer's prompt we decide afterwards, and users will decide theirs for their use case.

What this means for the rest of the log (pm's reading, confirmed with the owner):
- **The deliverable is a general engine mechanism.** One task can run several stages at once, and a join lets a later stage use all of their results. It isn't tied to any use case.
- **Out of scope:** reviewer roles, the lead's job, and every prompt. They are example content that a user writes, or that we write later.
- **The review panel becomes one example workflow** that exercises the mechanism. Q2's answer still applies to it: experimental, under `workflows/experimental/`, loaded by a test, not embedded.
- **Q3's options are dropped**, along with pm's open questions on overruling, anchoring and the lead's merge logic.
- **Mechanism questions still open:**
  - the YAML shape;
  - join semantics;
  - failure, cancel, retry and restart;
  - loop guards and outcomes per branch;
  - worktree access (read-only branches vs writers);
  - how templates address a branch's result;
  - the dashboard and `task status`;
  - cost per branch;
  - resource limits (concurrent builds);
  - claude/omp parity.

## Q4 (arch-panel): what does a parallel group look like in YAML?

Context from the code. A stage's report is stored under `payload.stages.<stage name>` and read with `{{ stages.<name>.<field> }}`. Sessions, usage rows and the `lap` counter are all keyed by stage name. If every branch is a uniquely named stage, then capture, templates, cost per branch and the session history work unchanged. Only "which stages are running" is new.

**A. Inline branches.** The group is one stage of a new kind, and its branches are full stage definitions nested inside it. Each branch has its own name, which is unique across the workflow. A branch declares the outcomes it may report, but it has no `on:` of its own: only the group routes.

```yaml
review_panel:
  kind: parallel
  branches:
    security_review:
      kind: agent_turn
      role: security
      prompt_file: prompts/security.md
      capture: json
      outcomes: [clean, blocking]
    ops_review:
      kind: agent_turn
      role: ops
      prompt_file: prompts/ops.md
      capture: json
      outcomes: [clean, blocking]
  on: { done: lead_review }          # how the group's own outcome is chosen is Q5
lead_review:
  kind: agent_turn
  prompt_file: prompts/lead.md       # reads {{ stages.security_review.summary }}
  ...
```

- For: a branch can only be entered through its group, by construction. The group reads top to bottom. Templates keep today's syntax.
- Against: a branch can't be reused outside its group. Nesting adds a level to the loader.

**B. A group that names top-level stages:** `kind: parallel`, `stages: [security_review, ops_review]`. The named stages are ordinary top-level stages.

- For: the stages look exactly like stages do today.
- Against: a branch stage's `on:` would have to mean "allowed outcomes, no routing", a different meaning for the same key. And the loader must forbid every other way into those stages.

**C. General fork and join edges.** An `on:` edge may list several targets (`done: [a, b, c]`), and a `join` stage waits for named stages. Any graph shape is possible.

- For: the most general.
- Against: a task then has a *set* of current stages everywhere. Every single-stage assumption is affected, not just one stage kind. Branches can diverge, loop and re-join at different points. Most of the cost, for shapes no use case has asked for.

**Recommendation: A.** Only agent turns, shells and polls are allowed as branches, and Q7 settles which of them. A branch can't itself be a group, a human gate or a terminal.

(From here on, the owner answers in arch-panel's tab directly.)

**Owner's answer (2026-10-09): A, inline branches.** A new `parallel` stage kind. Its `branches:` are uniquely named stages written inside it. Each branch declares the outcomes it may report, and none of them route; only the group has `on:`. Branches are limited to `agent_turn`, `shell` and `poll`: no nested groups, gates or terminals.

## Q5 (arch-panel): when does a group finish, and which outcome does it route on?

Context. Every branch ends by reporting one of its declared outcomes. The group then needs one outcome of its own to pick an `on:` edge. Two sub-choices.

**When it finishes.** Either it waits for every branch, or it stops early: the first branch that reports a chosen outcome (say `blocking`) cancels the rest. Stopping early saves the unfinished branches' cost, but it throws away paid partial work. The next lap then gets only one perspective's findings. That is the #95 lesson: a review that stops at its first blocking finding costs extra laps later.

**Which outcome.** Either the group always reports `done` and a following stage (a lead, say) decides, which costs one more agent lap, or the group carries ordered rules over its branches' outcomes:

```yaml
review_panel:
  kind: parallel
  branches: { ... }
  route:                       # first match wins; no match → done
    - any: blocking
      then: changes_requested
  on: { done: open_pr, changes_requested: revising }
```

Options:
- **A. Wait for all. Optional rules (`any:` / `all:` of a branch outcome); with no rule matching, the outcome is `done`.** A user can route with no extra lap, or send `done` to a lead stage that decides.
- **B. Wait for all, always `done`.** A later stage decides every time. Simplest engine, and there's always an extra lap when routing is needed.
- **C. A + early stop.** A rule may also say `cancel_rest: true`, and the first match cancels the still-running branches.

Recommendation: A. Early stop can be added later without changing A's YAML.

**Owner's answer (2026-10-09): B. Wait for all, and the group always reports `done`.** Every routing decision belongs to a following stage (for example a lead `agent_turn` with `capture: json`). A branch's outcome and report are information for later stages (`{{ stages.<branch>.outcome }}`, `.summary`). They never route.

## Q6 (arch-panel): what happens when one branch fails?

Context from the code. Today a turn that can't complete (no report, a crash, a lingering process, a read-only violation, a usage limit, the daemon stopping) parks the whole task as `stuck` with one reason. `choco task retry` re-enters the current stage, and it resumes the agent's session when the turn was cut off from outside. The engine keeps one invariant: `stuck` means nothing is running for the task (`advance_from_stage`'s watcher handling). Cancel already kills every session the task ever had and every detached runner, so it covers branches unchanged. The restart sweep parks an interrupted turn as `stuck` and resumes polls.

With N branches, one can fail while the others are still working.

- **A. Let the others finish, then park.** Siblings keep running. When every branch has settled, the task parks `stuck`, naming each failed branch and why. A failure is put on the timeline the moment it happens, so it's visible early. Retry re-runs only the failed branches; a branch cut off from outside resumes its session. Finished branches keep their reports and aren't paid for again. A restart works the same way: interrupted branches are parked, and retry resumes them.
- **B. Fail fast.** The first failure cancels the running siblings and parks the task immediately. Retry re-runs the whole group. Simpler state, but the siblings' paid work is thrown away and every branch is paid for again.

Recommendation: A. It keeps "`stuck` means nothing is running" and never pays twice for a finished branch.

**Owner's answer (2026-10-09): A. Let the others finish, then park.** Siblings keep running, and the failure is put on the timeline at once. The task parks `stuck` only when every branch has settled, naming each failed branch. Retry re-runs only the failed branches, or resumes them when they were cut off from outside. Finished branches keep their reports and aren't paid for again. A daemon restart is handled the same way.

## Q7 (arch-panel): may a branch change the worktree?

Context. All branches share the task's one worktree. Two branches writing to it at once corrupt each other's work. There's a subtler problem too. The read-only check (#172) snapshots the whole worktree before a read-only turn and compares it after. If any sibling changes a tracked file meanwhile, an innocent reader is blamed and parked. Shell and poll stages can't be declared read-only today: they are commands, and nothing stops a `cargo fmt` from rewriting tracked files. Build output in ignored folders (`target/`) is outside the check either way.

- **A. No writers in a group.** Every `agent_turn` branch must use a `read_only` role, checked at load time. Shell and poll branches are allowed. The worktree is also snapshotted when the group starts and compared when it settles, and any change parks the task naming the branches that were running. Ignored files are exempt, so builds and tests are fine.
- **B. Read-only agent turns only.** No shell or poll branches yet, so every branch is covered by the existing per-turn check, and the group needs no snapshot of its own.
- **C. Writers allowed, each in its own worktree,** with a merge step at the join. This is the general answer, but it means new worktrees, conflict handling and a merge policy. Better as a later phase, if a use case asks for it.

Recommendation: A. Running tests or a CI poll beside the reviewers is a natural use, and the group-level snapshot is the same mechanism the read-only check already has.

**Owner's answer (2026-10-09): A. No writers in a group.** Every `agent_turn` branch must use a `read_only` role, checked at load time. Shell and poll branches are allowed. The group snapshots the worktree when it starts and compares when it settles. A change to a tracked file parks the task, naming the branches that ran. Ignored files are exempt. Writers in their own worktrees are a possible later phase, if a use case asks for it.

## Q8 (arch-panel): how is concurrency bounded?

Context. The daemon has no concurrency limit of any kind today: not per task, not across tasks. Each task runs one stage at a time only because a task has one current stage. What a branch costs in disk and CPU depends on its prompt, not the engine: today's reviewer prompt clones HEAD into a scratch dir and builds it, 3–5 GB here. A group of N such branches holds N builds at once. Across tasks the risk already exists (4 tasks plus 4 reviewers filled the disk once); a group multiplies it within one task.

- **A. A per-group cap.** `max_concurrent: N` on the group. Branches beyond N wait and start as running ones settle, in declaration order. With no cap, all branches start at once. The workflow author, who knows what the branches do, sets it. Small and local.
- **B. A daemon-wide cap** on concurrently running agent turns (global config), across all tasks. A turn waiting for a slot shows as queued. It protects the machine whatever the source, but it adds a "waiting for a slot" state to every stage of every workflow, including today's single-stage ones. It's a feature of its own.
- **C. No cap.** Document the cost; the author decides N by how many branches they write.

Recommendation: A in this feature. The cross-task limit (B) isn't caused by this feature and stands or falls on its own.

**Owner's answer (2026-10-09): C. No cap.** The engine starts every branch of a group at once. The cost is documented, and the author bounds it by how many branches they write. A daemon-wide cap isn't part of this feature.

## Q9 (arch-panel): when the task comes back to a group, which branches run again?

Context. A group is entered again on every lap that routes back to it, for example after `revising`. Each branch is a paid turn. Under Q5, branches don't route, so the engine knows each branch's last outcome but doesn't interpret it.

- **A. Every branch runs again, every time.** Simple, and every report describes the current code. A lap costs N turns.
- **B. A branch can opt out of re-running.** For example, `rerun: { unless_last: [clean] }` keeps the last report of a branch whose previous outcome was `clean`. Cheaper laps, but the kept report describes code that has since changed, and the engine can't tell whether the change matters to that branch.

Recommendation: A for now. B can be added later as an optional per-branch key, without changing any YAML written for A.

**Owner's answer (2026-10-09): A. Every branch runs again, every time the group is entered.** An opt-out per branch can be added later as an optional key.
