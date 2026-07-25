---
name: implement-issue
description: Implement a ChocoFactory GitHub issue end-to-end — confirm scope and dependencies, get a short design approved, implement, run the fmt/clippy/test gate, self-check for this repo's recurring review findings, get an independent subagent reviewer to formally approve the diff, and open a draft PR. Use when picking up a specific issue number to build.
---

# Implement Issue

Argument: a GitHub issue number (`$1`). If not given, ask which issue.

## 1. Load and confirm scope

- `gh issue view $1 --json number,title,body,labels,state,url`
- If `state` is not `OPEN`, stop and tell the user — don't implement a
  closed or duplicate issue.
- Parse the body for a `Depends on:` line. For each referenced issue,
  `gh issue view <dep> --json state`. If any dependency isn't closed,
  stop and ask the user whether to proceed anyway.
- If the issue maps to a task in `.agents/*/04-plan.md`, read that
  task's "Design ref" line and the corresponding section of
  `03-design.md` for constraints the issue body doesn't restate.

## 2. Branch

- `git fetch origin && git checkout -b <prefix>/issue-$1-<slug> origin/main`
  if not already on an isolated branch/worktree for this issue.

## 3. Short design, then stop for approval

- Before writing code, post a short LLD as a comment on the issue:
  approach, data structures/interfaces touched, edge cases considered,
  open questions. `gh issue comment $1 --body "..."`.
- State the same LLD in chat and wait for explicit user approval before
  implementing — this mirrors the design-approval gate in
  `.agents/SOP/SpecDrivenDev.md` Step 3. Do not skip this for anything
  touching concurrency, shared state, or a public interface.
- Trivial, single-file mechanical changes may skip the issue comment but
  still get a one-line design statement in chat for the user to confirm.

## 4. Implement

- Make the change. Add a regression test for every bug fix; add
  unit/integration tests for new behavior.
- Keep the diff scoped to the issue — don't bundle unrelated cleanup.

## 5. Verification gate (must pass before opening a PR)

Run in order, fixing failures before proceeding:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

## 6. Self-check for this repo's recurring findings

This repo's review history keeps flagging two specific defect classes.
Check for them explicitly before opening the PR — don't wait for a
reviewer to catch them. Re-read every changed function end-to-end, not
just the lines you touched:

- **Non-atomic state races**: any read-modify-write on shared or
  persisted state (files, locks, task state) — is the whole sequence
  atomic, or can a concurrent caller observe or interleave a partial
  update?
- **Swallowed errors**: any `Result`/`Option` that's discarded,
  defaulted away, or logged-and-dropped where the caller should see the
  failure.

## 7. Independent subagent review — must formally approve before opening a PR

Do not open the PR off your own self-check alone. Spawn a **read-only**
subagent (Read/Grep only — no Bash writes, no git mutations, no Edit) as
an independent reviewer, and iterate with it until it formally approves:

1. Spawn a fresh subagent (no memory of your implementation rationale —
   a new spawn each round, not a resumed one) with this brief:

   > Do a thorough, detailed review of this change before it becomes a
   > PR. Here is the issue: [issue title/body/#]. Here is
   > `git diff origin/main...HEAD`. Review it as if you'll be blamed for
   > anything you miss — read every changed function end-to-end, not
   > just the hunks. Specifically hunt for: non-atomic read-modify-write
   > sequences on shared/persisted state, swallowed or discarded errors,
   > other correctness bugs, security issues, and missing test coverage
   > for the new behavior.
   >
   > Classify every finding as **need-to-fix** (a real bug, security
   > issue, or missing coverage for a case that matters) or **nit**
   > (style, naming, optional polish). Formally state APPROVE if and
   > only if there are zero need-to-fix findings — nits alone do not
   > block approval. Otherwise state CHANGES REQUESTED and list the
   > need-to-fix items with file:line.

2. Share the subagent's findings with the user as you go (don't just
   silently loop).
3. Address every need-to-fix item. Nits are optional — use judgment, but
   don't let them stall the loop.
4. Re-run the verification gate (step 5) if you touched code.
5. Spawn a **new** subagent reviewer (fresh, not resumed) against the
   updated diff. Repeat from step 1.
6. If you reach **5 iterations** without an APPROVE, stop. Do not open
   the PR and do not keep looping. Escalate to the user: show the diff,
   the outstanding need-to-fix items from the last round, and what you
   tried across rounds, and ask how they want to proceed.

## 8. Open the PR

- `gh pr create --draft --title "..." --body "Closes #$1

<summary>"`
- `Closes #$1` so the issue auto-closes on merge.
- Report the PR URL and a one-line summary of what changed and what's
  still pending (CI, review, etc).

## Notes

- Never commit `.agents/` planning docs automatically — only when the
  user explicitly asks (per the SOP).
- `.github/workflows/claude-review.yml` runs a paid Claude review pass
  on every push to an open PR. Batch changes locally and get the
  verification gate green before pushing, rather than pushing
  speculative fixes to see what sticks.
