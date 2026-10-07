# Contributing to ChocoFactory

How to build ChocoFactory, run its tests, run the daemon by hand while developing, and cut a release. Back to the [README](README.md).

## Build

```
cargo build --workspace
```

Binaries land in `target/debug/`.

## Tests and the gate

Every change must pass all of these before review:

```
cargo build --workspace --all-targets   # test harnesses spawn these binaries
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Build all targets first. On a cold tree `cargo test --workspace` runs only
about 89 tests across 2 suites and still exits 0, because the harnesses that
spawn the binaries aren't built yet. A suite count below 10 means a short
run, not deleted tests.

`jq` must be installed to run `cargo test --workspace`: the `gh` stubs in
`chocofactoryd/src/engine/tests.rs`, `chocofactoryd/tests/e2e_smoke.rs` and
`chocofactoryd/tests/await_review_script.rs` apply `-q` filters with it, so
those tests fail on a machine without it.
CI has it.

Tests never spawn the real `claude` — the integration suites point the
daemon at `mock-claude` or a Python fixture instead. `scripts/probe-setting-sources.sh` is an opt-in
check against the real CLI that `local` settings don't leak into a linked
worktree; it costs a little (two tiny haiku turns) and is not part of CI.

## Running the daemon by hand

Logs go to **stderr**, not stdout: redirect with `chocofactoryd 2> daemon.log`
(a plain `> log` captures nothing).

> **`chocofactoryd` spawns the real `claude` CLI by default** — running the
> daemon will hit the real, billable `claude` unless you point it at a
> stand-in first.

For manual testing, use the bundled `mock-claude` stand-in:

```
CHOCOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude ./target/debug/chocofactoryd
```

`mock-claude` echoes back whatever it's sent (`echo:{text}`); set
`MOCK_CLAUDE_REPLY=<text>` to get a fixed reply instead. Point
`CHOCOFACTORY_CLAUDE_BINARY` at the real `claude` binary only when you
specifically mean to exercise the real CLI.

The daemon stores its database under `~/.config/chocofactory/`. The
built-in workflows (`chat`, `coding-task`, `coding-task-planned`) come from the daemon binary: at
every start it regenerates a private, read-only copy in
`~/.config/chocofactory/.builtin-workflows/`, so upgrading the binary
upgrades them. See [Project workflows](docs/workflows.md#project-workflows) for where a
workflow can come from. To keep a test run fully isolated from your real
state, override `HOME`:

```
HOME=$(mktemp -d) CHOCOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude \
  ./target/debug/chocofactoryd
```

For what the lock file, stop and restart do, see
[docs/cli.md](docs/cli.md#the-daemon).

### Daemon environment variables

| Variable | Purpose |
|---|---|
| `CHOCOFACTORY_CLAUDE_BINARY` | Path to the agent CLI. Unset = the real, billable `claude`. |
| `CHOCOFACTORY_OMP_BINARY` | Path to the `omp` CLI used by roles with `cli: omp`. Unset = `omp` from `PATH`. |
| `CHOCOFACTORY_CHOCO_BINARY` | Path to `choco`, used to serve every agent turn's `report_outcome` tool (see [Routing on an agent's verdict](docs/workflows.md#routing-on-an-agents-verdict)). Unset = the daemon's own sibling `choco` binary. |
| `CHOCOFACTORY_PORT` | Bind port. Defaults to `4141`. Useful when a daemon is already running there. `0` picks a free port; the bound port is written to the lock file. |
| `MOCK_CLAUDE_REPLY` | Read by `mock-claude` only — reply with this fixed text instead of echoing. |
| `MOCK_CLAUDE_REPORT` | Read by `mock-claude` only — the JSON input of the `report_outcome` call a single-shot turn makes (default `{"outcome": "done"}`). |
| `RUST_LOG` | Log filter, e.g. `error` to quiet startup, `debug` for detail. |

## Releasing

Maintainers: the tag must equal `v` + the workspace version and be on `main`,
and the binaries report the workspace version, so a release candidate needs its
own version. The workspace version is kept at `X.Y.Z-rc.1` until the final
release. To cut `X.Y.Z`: merge at `X.Y.Z-rc.1`, push `vX.Y.Z-rc.1` (a
prerelease) and check its assets; then merge a one-line change setting the
version to `X.Y.Z` (and `Cargo.lock`) and push `vX.Y.Z`. While only a prerelease
exists, `releases/latest/download/*` returns 404, so install the rc with
`CHOCO_VERSION=X.Y.Z-rc.1` and the versioned script, e.g.
`curl -fsSL <RELEASES>/download/vX.Y.Z-rc.1/install.sh | CHOCO_VERSION=X.Y.Z-rc.1 sh`.
A copy installed from the rc moves to the final release with `choco update`. **Actions → Release → Run
workflow** (`workflow_dispatch`) is a dry run: everything except publishing.
