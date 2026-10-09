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
