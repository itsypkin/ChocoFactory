# ChocoFactory

## Memory vault

Durable memory for this repo lives in an Obsidian vault at `~/obsidian_vault/agent-memory`. It holds
the design rationale and the accumulated scar tissue from issues #67–#95 — things not recoverable from
the code.

Before starting work, read in this order and **stop as soon as you have the answer**:

1. `00_System/AI/RETRIEVAL_PROTOCOL.md` — the reading protocol itself
2. `20_Projects/chocofactory/00_meta/current-focus.md` — verified baseline, in-flight work, open issues
3. the context capsule matching the task domain:

| Capsule (under `20_Projects/chocofactory/00_meta/context-capsules/`) | Read before |
|---|---|
| `capsule-build-test` | building, testing, or starting the daemon |
| `capsule-database` | migrations, schema, persistence bugs |
| `capsule-workflows` | authoring or debugging a workflow YAML |
| `capsule-cli-operations` | driving, inspecting, or unsticking a task |

Most tasks resolve there. Escalate only if the answer wasn't in the capsule:
`20_Projects/chocofactory/00_meta/` for the repo/architecture/authority/dependency maps,
`20_Projects/chocofactory/wiki/` for engine, session, event and known-gap detail,
`20_Projects/chocofactory/decisions/index.md` for why a choice was made,
`20_Projects/chocofactory/specs/design-digest.md` to find the right section of
`.agents/ChocoFactory/03-design.md`.

**Repo truth overrides the vault.** On a conflict: say so, trust the repo for the task at hand, mark
the stale note, and correct it only from confirmed evidence — never from inference.

At session end follow `00_System/AI/SESSION_CLOSEOUT_PROTOCOL.md`. Its default is **leave the vault
unchanged**; update only on an explicit trigger, and not for partial progress.

## The gate

Every change must pass all three before review:

```
cargo build --workspace --all-targets   # test harnesses spawn these binaries
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

**Build all targets first.** `cargo test --workspace` on a cold tree reports only ~89 tests across 2
suites and still exits 0, because the harnesses that spawn the binaries aren't built. A suite count
below 10 means a short run, not deleted tests.

If a timing-sensitive daemon test flakes under parallel load, check #98 and #85 before assuming you
broke it.

## Self-check before opening a PR

Reviews on this repo repeatedly flag the same two defects. Check your own diff for both:

- **Non-atomic state transitions** — a read-then-write on task/run status that another writer can
  interleave with. One write per fact.
- **Swallowed errors** — a `Result` dropped, logged-and-continued, or collapsed into a default where
  the caller can no longer tell the operation failed.

## Conventions

- Planning docs follow `.agents/SOP/SpecDrivenDev.md`. `00-rough-idea.md` and `01-idea.md` are raw
  captures — never refactor them after their phase closes.
- Migrations are append-only: add the next numbered file in `chocofactoryd/migrations/`, never edit an
  applied one.
- `workflows/` in this repo is the source of truth for the built-ins; they are embedded at build time.
  The daemon **never overwrites** an already-seeded copy in `~/.config/chocofactory/workflows/`, so
  refresh that directory before testing "the latest" or you'll chase defects that no longer exist.
- `CHOCOFACTORY_CLAUDE_BINARY` unset means the daemon drives the real `claude` — that is the intended
  behaviour, not a hazard to route around. `mock-claude` is for the test suites and for a deliberately
  isolated smoke run; never substitute it when the point is to exercise the real CLI, and never leave it
  set in a run whose result you intend to trust.
- Issues and design text written before the rename spell things with a `k` — `chokofactoryd`,
  `CHOKOFACTORY_*`, `.agents/ChokoFactory/`. Don't copy a path out of an old issue body.
