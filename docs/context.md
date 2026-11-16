# Task context lookup

`GET /tasks/{id}/context` answers one question about a task, read-only: the
task's own facts, or one applied stage result. It never returns a transcript or
a list, writes nothing, and cannot complete a turn or report an outcome.

Use the daemon address your `choco` talks to (`127.0.0.1:4141` is only the
default; with `CHOCOFACTORY_PORT` or a lock file it differs). The examples use
`$DAEMON` for it.

Agents read it through the `get_context` tool (see [The agent tool](#the-agent-tool)
below); `choco` itself has no command for it. A bare HTTP caller supplies
`anchor_id` itself when it wants a fixed point in the history.

## What is recorded

Every time a task *actually leaves* a stage — an ordinary outcome, a parallel
group whose branches are all done, or a retry that sends a task from a gate
back to the watcher that timed out — the daemon writes one `stage_result`
event in the same database transaction as the move. Its ID is the result's ID.
It holds:

| field | meaning |
|---|---|
| `source_stage`, `source_kind` | the stage left, and its kind (`agent_turn`, `shell`, `poll`, `human_gate`, `parallel`) |
| `outcome` | the outcome applied. A group leaves with `done`; the gate-to-watcher retry is `retry` |
| `destination_stage` | where the task actually went, after any loop-guard redirect |
| `capture_present`, `capture` | the full value handed to the transition. `capture_present: true` with `capture: null` is a captured JSON null; `false` means nothing was captured (none declared, or over the 1 MiB capture limit) |
| `previous_result_id` | the result before this one, or `null` for the first |

The newest result's ID is kept in the task's workflow payload as
`current_result_id`. History is ordered only by these links, never by
timestamps or ID order. A task that has not left a stage yet, and every task
from before this was recorded, has no results; nothing is back-filled.

Stage results are pruned with the other events (the retention job), so a link
can point at a result that is gone. The lookup says so (`history_gap`) rather
than guessing. Capture text is not checked for truth, and the daemon does not
look at the worktree.

## Query

`source` is required: `task` or `stage`.

| request | meaning |
|---|---|
| `source=task` | the task's title, input and closing issues; no other key |
| `source=stage&stage=NAME` | the latest result that left stage `NAME` |
| `…&outcome=VALUE` | only a result with exactly that outcome |
| `source=stage&result_id=ID` | exactly that result (no `stage`, no `outcome`) |
| `…&anchor_id=ID` | fix the point in history: only results at or before this one |

Without `anchor_id`, a `stage` request starts at the task's current result,
read once for the whole request. With one, it starts there even if the task has
moved on since. IDs are opaque full IDs; `stage` and `outcome` match exactly.

Anything else is `400` with `{"error": "…"}`: a missing or unknown `source`,
keys mixed across forms, `stage` together with `result_id`, an empty or
whitespace-only value, a repeated key, an unknown key (including `stage[]`).
A query string has no JSON types: `stage=null` and `stage=7` are just the names
`null` and `7`.

## Responses

Every answer about a task that exists — each `200`, and the `404` for an ID
that cannot be read — has `provenance` (`task`, `stage_result`, `snapshot` or
`unavailable`), `task_id` and `anchor_id` (the anchor used, or `null`). The one
exception is a task that does not exist: that `404` is the API's usual
`{"error": "…"}` body, with none of these fields.

```
$ curl -s "$DAEMON/tasks/$ID/context?source=task"
{"provenance":"task","task_id":"…","anchor_id":null,
 "task":{"title":"Fix the thing","input_available":true,
         "input":"Private brief for the task.","closes":[369]}}
```

`input` comes only from what the task was started with. If that is not
recorded, `input_available` is `false` and `input` is `null`.

```
$ curl -s "$DAEMON/tasks/$ID/context?source=stage&stage=review&outcome=changes_requested"
{"provenance":"stage_result","task_id":"…","anchor_id":"<current result>",
 "result":{"task_id":"…","result_id":"…","source_stage":"review",
           "source_kind":"human_gate","outcome":"changes_requested",
           "destination_stage":"coding","capture_present":true,
           "capture":"Make exactly these changes.","previous_result_id":"…",
           "created_at":"2026-11-15T10:00:00Z"}}
```

`created_at` records when the event was written; it is not used to order
anything.

A task from before results were recorded can answer a `stage` request with its
latest stored capture, marked as such. This is the only fallback, and it needs
no `anchor_id`, no `result_id`, no `outcome`, and no `current_result_id`:

```
{"provenance":"snapshot","task_id":"…","anchor_id":null,"stage":"review",
 "capture_present":true,"capture":{…}}
```

Anything else that cannot be answered is
`{"provenance":"unavailable", …, "reason":"…"}`, with no capture in it:

| reason | when |
|---|---|
| `missing_workflow_state` | the task has no workflow state (a `stage` lookup by name) |
| `missing_result` | `result_id` is not a stage result of this task (**404**) |
| `missing_anchor` | the anchor — supplied (**404**) or the task's current result (200) — is not a stage result of this task |
| `history_gap` | a link needed to answer cannot be read (pruned, or not this task's) |
| `not_found_in_recorded_history` | the chain ends without a match. This does not say nothing happened earlier |
| `not_at_or_before_anchor` | the whole chain from the anchor was read and does not contain that result |
| `missing_snapshot` | pre-results task with no capture for the stage (and no `outcome` asked for) |
| `unverifiable_snapshot_outcome` | pre-results task asked for an `outcome`: a stored capture cannot say which outcome was applied, so this is answered whether or not a capture exists |
| `malformed_history` | a pointer, result or link has an unexpected shape, or the chain loops |

An ID that is missing, pruned, another task's, a session's, or of another event
type gets the same `404` response: the endpoint never says whether it exists
anywhere else. An unknown task is the usual `404 {"error": …}`. A storage
failure is a `500`, never an "unavailable".

## Reads are consistent

All reads of one request happen in a single read-only database transaction, so
the pointer, the captures and the chain come from the same moment. While it
runs, a writer waits for it to finish (SQLite's default journal mode); a
dropped request releases it.

## The agent tool

Every agent turn the daemon launches gets a read-only `get_context` tool, on
both `claude` and `omp`, bound at launch to that turn's own task and to the
point in history it started from. It returns one JSON value with the same
shapes as above, in full. A read never completes, reports or advances a turn.
A workflow stage's turn is accepted only through its own `report_outcome`; a
standing chat session has no report and its turns end as they always did.

| call | returns |
|---|---|
| `{"source":"task"}` | the task's title, input and closing issues |
| `{"source":"stage","stage":"NAME"}` (optionally `"outcome":"VALUE"`) | the latest recorded result that left that stage |
| `{"source":"stage","result_id":"ID"}` | exactly that result of this task |

Nothing else is accepted: an unknown key (`task_id`, `anchor_id`,
`session_id`, `daemon_url` and so on), a blank or non-string value, or keys
from different forms mixed together is a tool error, so the model cannot steer
the tool to another task, anchor or address. `stage` and `outcome` are matched
exactly; `outcome` must not be blank (a result recorded with an empty outcome
can still be fetched by `result_id`).

The binding is fixed when the turn starts, and the daemon builds it from what
it knows about that launch, never from a model's arguments:

- **task**, **session** and **address**: the task and session the daemon
  created or resumed for this turn, and the address it is actually listening
  on (so `CHOCOFACTORY_PORT=0` works). A workflow turn resumed after an
  interruption or a daemon restart starts a new session and is bound to that
  session and the current address. A standing chat session whose process is
  started again by a new message keeps its existing session: it is bound to
  that session, the stored boundary and the current address.
- **anchor**: the newest stage result recorded before the turn started, sent
  with every stage request. Results recorded later are invisible to it, so
  the answer to "what did the reviewer say" cannot change under a running
  turn. A later result's ID is `not_at_or_before_anchor` when the chain from
  the anchor is fully readable, and `history_gap` when a link has been pruned. A resumed or
  retried turn gets the same anchor again unless the task has since moved
  through a stage.

A turn that started before any result existed (no earlier departure) has an
*empty boundary*, and the tool does not read results recorded later. Both
stage requests then answer `unavailable` with `anchor_id: null` and reason
`not_found_in_recorded_history`, after checking that the task still exists
(a deleted task or an unreachable daemon is a tool error). This means no
matching result is established within that turn's recorded boundary; it does
not mean no earlier review happened. The stored snapshot a task from before
results were recorded may hold is never offered through the tool; the raw
HTTP lookup above still returns it. `source=task` always works.

A typed `unavailable` answer (HTTP 200, or the 404 for a missing result or
anchor) is a successful tool result. An unknown task's plain error body, any
other HTTP status, an answer that is not one of the shapes above, a refused or
dropped connection and a request that takes longer than 30 seconds are tool
errors, never an empty result. The tool makes one request per call (an
empty boundary asks only for the task), follows no redirect (a 3xx status is a
tool error), and an answer that omits a field the daemon always writes or
contradicts itself (for example a `capture` with `capture_present: false`, a
result whose `task_id` differs from its answer's, an answer about another task
than the bound one, an `anchor_id` on a task or snapshot answer, or an empty
stage name) is a tool error too. There are no retries, and nothing is written.

`choco mcp-serve` (hidden, started by the daemon) takes the binding as
`--context-task-id`, `--context-session-id`, `--context-base-url` and exactly
one of `--context-anchor-id` or `--context-no-anchor`. Giving one requires all;
none means a report-only server that offers no `get_context`. There is no
environment or default-port fallback. Neither listing tools nor a report call
contacts the daemon, so an unreachable daemon cannot stop a turn reporting.
When `claude_settings` were not applied, only `claude_settings_rejected` is
offered, for list and call alike.
