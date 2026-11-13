# Runtime probes for "advance completed turns after verified cleanup"

Sanitized record of the two opt-in runs behind the change. Neither is part of
`cargo test`. They were run once each (a first attempt and a correction) on
2026-11-13 with branch-built `choco` and `chocofactoryd` 0.3.4 (dev build).
The deterministic tests are the proof of the rules; these runs only show the
change working against the real CLIs and the production 30-second grace.

Both scripts are in this folder in their final form (the paired one gained a
few extra query sections after attempt 1) with the checkout path now the
`REPO_ROOT` variable (macOS: they use `sed -i ''`, `lstart` and `$TMPDIR`).

```
REPO_ROOT=<checkout with target/debug/{choco,chocofactoryd}> ./scripted-cutoff-smoke.sh
REPO_ROOT=<same> ./paired-real-probe.sh      # spends one claude and one omp session
```

Isolation in both: a fresh `$TMPDIR/choco … <pid>` directory (the name has a
space) holds the repo, a private `HOME` and the probe files. Every `choco`
call goes through `ch()`, which aborts unless the private
`$HOME/.config/chocofactory/chocofactoryd.lock` exists and runs with
`CHOCO_BASE_URL` unset and that `HOME`; the daemon is started with
`server start --port 0` under the same `HOME`. The shared daemon is never
touched. On exit each script cancels its task, runs `server stop --force`,
checks `server status` exits 3 and greps `ps` for anything left under its
directory (all runs: nothing left).

## 1. Scripted usage-limit smoke (mock CLI; mechanics only, not model evidence)

`CHOCOFACTORY_CLAUDE_BINARY` is set only in that private daemon's start
environment, to a wrapper that runs a probe-only copy of
`chocofactoryd/tests/fixtures/fake_claude_script.py` (its 600 s sleep cut to
180 s) with `initial.json` or, when its arguments contain ` --resume `,
`resumed.json`. The initial script: read the turn, start an escaped child
(`setsid`, double fork), write `smoke-edit.txt`, report `done`, emit the usage
limit, sleep. The resumed script reports `done` again. The role is not
read-only so the edit can be checked afterwards.

| | attempt 1 (task `da132272-532d-4751-bdc4-9c285c8daa0e`) | attempt 2 (task `c1111711-100b-4d4a-832c-7828f87941ac`) |
|---|---|---|
| first session | `57d37f17` exited / interrupted / verified, adapter id `6a41442a-40c3-40f3-b8d7-5b58c15f9083` | `adaa6a81` exited / interrupted / verified, adapter id `e187d4a7-2897-41ee-b902-3a601bc3cb7b` |
| started → ended | 14:55:58.60 → 14:56:29.14 UTC (30.5 s: grace kill) | 14:56:43.29 → 14:57:13.81 UTC (30.5 s) |
| task | stuck: "…interrupted by a usage limit before completing; 'choco task retry' attempts to resume it when eligible" | same text |
| child | pid 1272 gone | pid 1708 seen alive by `ps` (ppid 1, pgid 1707) before the kill, gone after |
| `task retry --resume` | "Retrying stage 'coding' by resuming its interrupted session (6a41442a-…)" | same, with `e187d4a7-…` |
| new session | `6d4e221c` idle / no reason / verified, `resumed_from` = `57d37f17`, same adapter id | `db1d0cc2`, `resumed_from` = `adaa6a81`, same adapter id |
| stage entries | `coding`, `coding:retry_resume`, `finished:done` (once) | same |
| edit after retry | present | present |

Between the attempts the script gained the `ps` line and the final
`server status` check (so attempt 1's log lacks them); the outcomes matched.

Captured: all rows above. Inferred, not captured: that `--resume` was passed.
The wrapper logged argv cut to 300 characters and its `grep` for `--resume`
printed nothing, so the log does not show the flag. The inference rests on the
wrapper choosing `resumed.json` and the fixture taking its session id from
that flag: the new session's adapter id equals the old one's.

## 2. Paired real-model probe (claude 2.1.323 + omp 18.20.0)

Real CLIs, `CHOCOFACTORY_CLAUDE_BINARY` unset. Launchers named `claude` and
`omp` on the private daemon's `PATH` only restore the operator `HOME` and
`exec` the resolved absolute binaries (`~/.local/share/claude/versions/2.1.323`,
`~/.local/bin/omp`), passing every argument unchanged. Workflow
(`paired.yaml` in the script): `worktree: true`; `claude_reviewer` =
`claude-sonnet-5-5:low`, `omp_reviewer` = `openai-codex/gpt-6.1-sol:low`, both
`read_only: true`; parallel stage `panel` with two `agent_turn` branches,
`capture: json`, `results: [changes_requested]`, `on: {done: finished}`;
default isolation. The exact prompts are the `claude.md` and `omp.md` heredocs
in the script. In short: Claude runs one foreground Bash command with
`timeout` 8000 ms and then reports `changes_requested` / `claude-probe-5c1d`;
omp starts a named service (`probe-service`, `pty false`, no `ready`, no
`async`), must not poll, ends its turn without reporting, and after an
automatic wake reports `changes_requested` with the service's release token.
The controller writes the token and the release file only after it sees both
omp's first `turn_completed` and Claude's branch `done`.

**Attempt 1** — task `a8481082-5d07-4a46-907d-aa7cc94d857a`; sessions
`6ac9b418` (claude), `37e063f0` (omp). All times 2026-11-13 UTC.

- Claude: job pid 2945 (ppid 2942, pgid 2942, state S, started 14:58:41),
  seen by `ps` every second. Clean `turn_completed` 14:58:52.51;
  `lingered` note 14:59:22.60 (30.09 s later), then `leftovers_killed` at
  14:59:22.62; job `gone` at the next `ps` sample (14:59:23). Session idle /
  no reason / verified. `turn_outcome` applied, `changes_requested`,
  `claude-probe-5c1d`, at 14:59:22.77.
- omp: service pid 2982 (ppid 2981, pgid 2982, `Ss`, started 14:58:44).
  First `turn_completed` (report-less) 14:58:53.61. Release and fresh token
  `tok-d84b4435` at 14:59:23 (after Claude's branch was `done`). Wake:
  `execution_started` 14:59:23.77. Later `turn_completed` 14:59:37.18, then
  one `leftovers_killed` at 14:59:37.30. Session idle / no reason / verified.
  `turn_outcome` applied once at 14:59:37.48 with summary
  `omp-probe tok-d84b4435`.
- Group: `finished_stages` lists both branches and `panel`;
  `parallel_entries.panel` is 1; stage entries `panel`, `finished:done`; task
  `closed`. The session-note rows of the task contain no `nudge` note.
  Server stopped (exit 0), `server status` 3, nothing left under the probe
  directory.

**Attempt 2** (the correction; task
`352ed366-b53b-405d-9db4-8f1cc226f252`, sessions `d4abbd9f` claude,
`53cbbc74` omp): the Claude half repeated. The tool result read "Command did
not complete within its 8s timeout and was moved to the background (ID: …)";
clean end 15:00:11.95, `lingered` note 15:00:42.07, applied 15:00:42.28,
verified. The omp branch did not follow its prompt: it called `pidwait`,
read the service script and `token.txt`, and reported at 15:00:29 without a
wake and before any release (`released=0`). Not counted as wake proof.

**Not captured, so not claimed:** the argv choco gave each CLI (the adapters
build it; nothing logged it); omp's `yielded` / `isTerminal` /
`awaitingAsyncWork` flags; any prompt-count log (only "no `nudge` note" was
checked, the one path that sends a second prompt); the Claude tool-result text
in attempt 1 (attempt 2 has it); the probe databases (deleted with the probe
directories, so the ids above are labels only). The paid budget (two paired
attempts) is used. This is one successful wake and one successful grace
cleanup, not a statistical claim, and it does not close the linked issues.
