---
name: resolve-reviews
description: Resolve one round of PR review comments safely — re-checks the PR isn't already merged, pulls every unresolved comment from any author (including claude[bot]), fixes them with a race/error self-check, verifies with a read-only subagent, then pushes. Use when addressing review feedback on an open ChocoFactory PR.
---

# Resolve Reviews

Argument: a PR number (`$1`). If not given, ask which PR.

## 1. Check the PR is still open

- `gh pr view $1 --json state,mergedAt,headRefName,baseRefName`
- If `state` is `MERGED`, **stop immediately**. Do not push to a dead
  branch. Tell the user, and propose cherry-picking the intended fix
  onto `main` in a new follow-up PR instead.
- If `state` is `CLOSED` (not merged), stop and ask the user what they
  want to do.
- Otherwise, `git fetch origin && git checkout <headRefName>` (or the
  matching worktree) before making any changes.

## 2. Fetch every review comment — do not filter by author

- `gh api repos/{owner}/{repo}/pulls/$1/comments --paginate`
- `gh api repos/{owner}/{repo}/pulls/$1/reviews --paginate`
- Pull ALL comments/reviews regardless of who posted them — human
  reviewers and bots (e.g. `claude[bot]`) alike. Do not add an author
  filter here: a past bug in this repo's tooling matched the login
  `claude` and silently missed `claude[bot]`, dropping a real review
  comment. If you ever do need to distinguish bot vs. human, match on
  `user.type == "Bot"`, never on a login substring.

## 3. Build a checklist before touching any code

List every comment as `file:line — required change — status (open)`.
Show it to the user before starting fixes so nothing gets silently
skipped.

## 4. Fix each item

- Address one checklist item at a time.
- Add a regression test for every bug the review comments identified.
- After each fix, re-read the checklist item and confirm the change
  actually addresses what was asked, not just something nearby.

## 5. Verification gate

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace
```

## 6. Self-check for this repo's recurring findings

Review rounds in this repo have specifically reintroduced these two
defect classes while "fixing" something else — check explicitly, don't
rely on the next review round to catch your own regression:

- **Non-atomic state races**: does your fix split a previously-atomic
  read-modify-write, or change lock/eviction ordering in a way that lets
  a concurrent caller observe a partial state?
- **Swallowed errors**: did your fix add a new discarded `Result`,
  `unwrap_or_default`, or silent log-and-continue on an error path?

## 7. Independent verification before pushing

Spawn a **read-only** subagent (Read/Grep only — no Bash writes, no git
mutations) with this brief:

> You are a hostile Rust reviewer with no knowledge of why these changes
> were made. Here is the checklist of review comments: [paste]. Here is
> `git diff origin/<base>...HEAD`. For each checklist item, mark
> ADDRESSED / PARTIAL / MISSED with the exact file:line that resolves
> it. Separately, scan the whole diff for: reintroduced race conditions,
> non-atomic write sequences, lock acquisition/eviction ordering
> changes, resource leaks on early return, and newly swallowed errors.
> Report findings as file:line + severity, or reply CLEAN.

If the critic reports anything, fix it and re-spawn a **fresh** critic
(no memory of the prior round). Repeat up to 3 times before pushing.

## 8. Re-check merge state, then push

- Re-run `gh pr view $1 --json state,mergedAt` — state can change while
  you were working. Abort per step 1 if it's now merged.
- Push, then post one summary comment on the PR mapping each original
  checklist item to the commit SHA that resolved it.

## Notes

- `.github/workflows/claude-review.yml` triggers a paid Claude review
  pass on every push to this PR. Batch all checklist fixes and get the
  verification gate + subagent critic clean locally before pushing —
  don't push once per comment.
