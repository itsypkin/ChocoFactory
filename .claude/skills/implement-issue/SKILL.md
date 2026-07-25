---
name: implement-issue
description: Implement a ChocoFactory GitHub issue end-to-end — confirm scope and dependencies, get a short design approved, implement, run the fmt/clippy/test gate, self-check for this repo's recurring review findings, and open a draft PR. Use when picking up a specific issue number to build.
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

## 7. Open the PR

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
