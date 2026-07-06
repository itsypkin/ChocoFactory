# 05 — Parallel Execution Plan

Dependency-graph analysis of the open GitHub issues (`itsypkin/ChocoFactory`),
grouped into waves of work that can proceed in parallel. Derived from each
issue's `Depends on:` line as of 2026-07-06. #1 and #2 are already closed.

## Dependency graph (open issues)

| Issue | Title | Depends on |
|---|---|---|
| #3 | P1-3: Agent adapter abstraction + Claude adapter | #1 (closed) |
| #4 | P1-4: Session lifecycle manager | #2, #3 |
| #5 | P1-5: Event capture + retention job | #2, #3 |
| #6 | P1-6: Workflow definition loader | #1 (closed) |
| #7 | P1-7: Workflow engine core (Phase-1 stage kinds) | #2, #4, #6 |
| #8 | P1-8: Built-in chat workflow | #7 |
| #9 | P1-9: HTTP/WS API layer | #2, #4, #5, #8 |
| #10 | P1-10: choco CLI | #9 |
| #11 | P1-11: Web UI — navigation, live chat, event timeline | #9 |
| #12 | P2-1: shell stage kind | #7 |
| #13 | P2-2: poll stage kind | #7 |
| #14 | P2-3: Cross-stage templating | #12, #13 |
| #15 | P2-4: Loop guards | #7 |
| #16 | P2-5: Worktree manager | #2 |
| #17 | P2-6: Multi-role config resolution | #8 |
| #18 | P2-7: Built-in coding-task workflow | #12, #13, #14, #15, #16, #17 |
| #19 | P2-8: Task delegation end-to-end | #10, #18 |

## Waves (parallelizable groups)

Each wave can be worked on concurrently once the previous wave completes.

| Wave | Issues | Unblocked by |
|---|---|---|
| 1 | #3, #6, #16 | #1/#2 (already closed) — ready to start now |
| 2 | #4, #5 | #3 |
| 3 | #7 | #4 and #6 |
| 4 | #8, #12, #13, #15 | #7 |
| 5 | #9, #17, #14 | #8 (for #9, #17); #12+#13 (for #14) |
| 6 | #10, #11, #18 | #9 (for #10, #11); #12/#13/#14/#15/#16/#17 (for #18) |
| 7 | #19 | #10 and #18 |

## Observations

- **#7 (Workflow engine core) is the critical bottleneck.** Nearly all of
  Phase 2 and half of Phase 1 fan out from it; nothing in wave 4 onward can
  start until it lands.
- **#16 (Worktree manager) is independently startable today** — it only
  depends on #2 (closed) despite not being consumed until #18, much later.
  Good filler task for a spare contributor while the wave-1/2/3 chain is
  in progress.
- **Critical path** (longest chain, bounds minimum total time regardless of
  parallelism): #3 → #4 → #7 → #8 → (#9/#17 or #12/#13/#14) → (#10 or #18)
  → #19 — roughly 7 sequential steps.
- With 3 available workstreams, #3, #6, and #16 can be picked up
  simultaneously right now.
