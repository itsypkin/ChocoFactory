#!/usr/bin/env python3
"""Stand-in for `omp --mode rpc`.

Speaks enough of omp's JSONL RPC protocol for the adapter: a `ready` frame
offering protocol versions [1, 2], `negotiate_protocol`, `set_event_filter`,
`set_host_tools`, `get_state`, `get_session_stats` and `prompt`. A prompt is
answered with assistant `message_end` frames carrying usage, a
`host_tool_call` for `report_outcome`, an `agent_end` that closes the run
(its `stopReason` is the run's status), a `prompt_result` and, when the
session was left unsettled, a `session_settled`. Every run is bracketed by
`agent_start` / `agent_end`; `set_event_filter` is honoured, so a session
that doesn't subscribe to those never sees them.

Behaviour is driven by environment variables (tests set them through a tiny
wrapper script, never through the test process's own environment):

- FAKE_OMP_REPORTS: JSON list of `report_outcome` argument objects to call
  in turn, stopping at the first the daemon accepts. Default: one `done`.
- FAKE_OMP_MODES: comma-separated flags, any of
  unpriced   assistant usage has cost.total 0 (a model with no price list)
  error      the run's final assistant message and the `prompt_result` are errors
             (FAKE_OMP_ERROR_MESSAGE, FAKE_OMP_ERROR_STATUS)
  unsettled  `agent_end` says background work is awaited and `prompt_result.sessionSettled`
             is false; the background run (its own start, message and end) and
             `session_settled` follow
  garbage    a malformed line and an unknown frame precede the turn
  bad_chunk  an invalid `rpc_chunk` sequence precedes the turn
  chunked    the final assistant message is sent as `rpc_chunk` frames
  die        the process exits mid-turn, before `prompt_result`
  u2028      the final assistant text contains a raw U+2028
  echo       the final assistant text is a JSON report of argv, env and the
             overlay file's contents
  noreport   no `report_outcome` call is made
  local_command  the prompt is a builtin command: response `agentInvoked: false`, no `prompt_result`
  side_calls the session statistics include FAKE_OMP_SIDE_TOKENS input
             tokens that no message carries
  discarded  before the final message, one extra empty assistant message with
             usage that omp then discards: it is not in the session
             statistics. Its model is the main one, or the second model with
             FAKE_OMP_DISCARD_SECOND=1
  second_model  before the final message, one extra assistant message from
             FAKE_OMP_SECOND_PROVIDER / FAKE_OMP_SECOND_MODEL (default
             other-provider / other-model), counted in the statistics
  no_messages  the prompt sends no message_end at all (its `agent_end` still
             carries a final assistant message); the statistics gain
             FAKE_OMP_SIDE_TOKENS input and FAKE_OMP_SIDE_COST cost
  client_file  at start, before anything else, write omp's "project
             presence" file `$XDG_STATE_HOME/omp/run/daemons/<hash>/clients/<pid>-<uuid>.json`
  no_ready   the process sleeps instead of speaking (never used for success)
  unknown_tool  the turn first calls a host tool the daemon never registered
                and reports the daemon's reply as assistant text
  stray_result  a `prompt_result` for a prompt id nobody sent precedes the turn
  exit_on_stats the process says one more thing and exits when the turn's
                statistics are requested (stdout ends while the daemon waits)
  unsettled_die `prompt_result` says the session is unsettled, then the
                process exits without `session_settled`
  reject_prompt the `prompt` response is `success: false` and no
                `prompt_result` follows (omp drops the ticket)
  reject_prompt_late  as `reject_prompt`, then a `prompt_result` error for
                the same id arrives anyway
  reject_followup the first prompt is accepted; the next prompt received
                (while it runs) is rejected with `success: false`, then the
                first prompt finishes normally
  reject_unsettled with `unsettled`: a prompt received after the unsettled
                `prompt_result` is rejected with `success: false`
  reject_noerror  as `reject_prompt`, but the response carries no `error`
  bad_host_call a `host_tool_call` with an id but no toolCallId or toolName
                precedes the turn; the daemon's reply is reported as text
  state_error   `get_state` fails
  tools_error   `set_host_tools` fails
  v1_only       `ready` offers only protocol 1; negotiating kills the process
  negotiate_error  `negotiate_protocol` fails (the session stays on v1)
- FAKE_OMP_SCRIPT: a JSON file holding a list of steps. The first agent
  prompt runs them instead of the canned turn (later prompts are only
  logged and answered; `/skill:…` is rejected, `/help` is a builtin command
  answered with agentInvoked false). FAKE_OMP_GATE_DIR names the directory whose files
  release `gate` / `hold_stats` steps; FAKE_OMP_LOG names a file that gets
  one progress line per step and per interesting frame, so a test can wait
  on what the fixture really did. Steps (`op`):
  agent_start / turn_end / session_settled / log {tag} / pid / sleep {seconds} /
  exit {code} / gate {name} (blocks, still answering requests, until the file
  exists) / hold_stats {gate} (the next get_session_stats reply is delayed
  until that gate file exists, and then reports the totals of that moment) /
  assistant {text, usage, counted, stop_reason, error_message, error_status}
  / report {args} (a real host call under fresh ids; waits for the reply) /
  agent_end {yielded, terminal, awaiting, messages} (`yielded` and
  `awaiting` take a JSON value or "ABSENT"; `messages` is last (default),
  empty, absent, string or unknown_reason) / prompt_result {status, settled,
  error_message, error_status} / add_totals {input, cost}.
- FAKE_OMP_STATS_SEQ: comma list of `ok|error|garbled|silent`, one per
  `get_session_stats` call (the last repeats). Default `ok`.
- FAKE_OMP_START_TOKENS: input tokens the session already has at start (a
  resumed session).
- FAKE_OMP_PROVIDER / FAKE_OMP_MODEL: what `get_state` reports.
- FAKE_OMP_NO_PROVIDER=1: omit the provider from `get_state` and from messages.
- FAKE_OMP_VERSION_FAIL: `--version` exits 1.
- FAKE_OMP_PROMPT_LOG: a file; every received prompt appends one JSON line
  `{"message": ..., "streamingBehavior": ...}` to it.
- FAKE_OMP_LAUNCH_LOG: a file; the process appends its start time (seconds)
  to it as its first act.
- FAKE_OMP_VERSION_LOG (read by the test wrapper, not by this script): see
  `wrapper_for` in omp_tests.rs.
- XDG_STATE_HOME: when set, the process records at start whether
  `$XDG_STATE_HOME/omp` is a directory; `echo` reports it as
  `state_omp_dir_at_start` (real omp ignores the variable if it isn't).
- FAKE_OMP_MEMORY_STATS: how the prompt `/memory stats` is answered (it makes
  no model call). The database path and bank are read from the `--config`
  file's `dbPath:` and `bank:` lines.
  healthy (default)  a `command_output` table row, then `agentInvoked: false`
  error       the response is `success: false`, `EACCES: permission denied`
  home_relative        the row shows the path as `~/…` against this HOME
  home_relative_other  the same for another project's store
  home_relative_default  the same for omp's default `~/.omp/…` store
  other_path  the row names `/elsewhere/mnemopi.db`
  other_bank  the row names bank `choco-1a2b`
  no_output   the response only
  agent       `agentInvoked: true`, then a `prompt_result`
  exit        the process exits on the prompt
  hang        the prompt is never answered
"""
import base64
import json
import os
import re
import select
import sys
import time

ARGS = sys.argv[1:]
MODES = set(filter(None, os.environ.get("FAKE_OMP_MODES", "").split(",")))
USAGE = {"input": 100, "output": 20, "cacheRead": 30, "cacheWrite": 5}
COST = 0.0123


if os.environ.get("FAKE_OMP_LAUNCH_LOG"):
    with open(os.environ["FAKE_OMP_LAUNCH_LOG"], "a") as launch_log:
        launch_log.write("%f\n" % time.time())


STATE_OMP_DIR_AT_START = None
if os.environ.get("XDG_STATE_HOME"):
    STATE_OMP_DIR_AT_START = os.path.isdir(os.path.join(os.environ["XDG_STATE_HOME"], "omp"))

if "client_file" in MODES and os.environ.get("XDG_STATE_HOME"):
    import uuid

    _client_id = str(uuid.uuid4())
    _clients = os.path.join(
        os.environ["XDG_STATE_HOME"], "omp", "run", "daemons", "e2f034ac33e93976", "clients"
    )
    os.makedirs(_clients, exist_ok=True)
    with open(os.path.join(_clients, "%d-%s.json" % (os.getpid(), _client_id)), "w") as _f:
        json.dump({"pid": os.getpid(), "id": _client_id, "projectDir": os.getcwd()}, _f)


def emit(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


FILTERABLE = {"agent_start", "agent_end", "turn_end", "message_end"}


def emit_event(obj):
    """An agent event: sent only if the session's event filter allows it."""
    if STATE.filter is not None and obj.get("type") in FILTERABLE:
        if obj["type"] not in STATE.filter:
            return
    emit(obj)


def log(tag):
    path = os.environ.get("FAKE_OMP_LOG")
    if path:
        with open(path, "a") as handle:
            handle.write(tag + "\n")


def flag(name):
    if name in ARGS:
        index = ARGS.index(name) + 1
        if index < len(ARGS):
            return ARGS[index]
    return None


def respond(request, data=None, success=True, error=None):
    frame = {
        "type": "response",
        "command": request.get("type"),
        "success": success,
    }
    if "id" in request:
        frame["id"] = request["id"]
    if success:
        frame["data"] = data
    else:
        frame["error"] = error
    emit(frame)


class State:
    def __init__(self):
        self.v2 = False
        self.stats_calls = 0
        self.totals = {
            "input": int(os.environ.get("FAKE_OMP_START_TOKENS", "0")),
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
        }
        self.cost = 0.0
        self.session_id = flag("--resume") or "omp-session-1"
        self.tools = (flag("--tools") or "").split(",")
        # The events `set_event_filter` asked for; None until it is sent.
        self.filter = None
        self.script_ran = False
        self.prompt_id = None
        self.report_n = 0
        self.last_assistant = None
        self.hold_next = None
        self.held = None


STATE = State()


def stats_mode():
    seq = os.environ.get("FAKE_OMP_STATS_SEQ", "ok").split(",")
    index = min(STATE.stats_calls, len(seq) - 1)
    STATE.stats_calls += 1
    return seq[index]


def handle_stats(request):
    log("stats_request")
    if STATE.hold_next:
        STATE.held = (request, STATE.hold_next)
        STATE.hold_next = None
        log("stats_held")
        return
    send_stats(request)


def service():
    """Releases a held statistics reply once its gate file exists."""
    held = STATE.held
    if held and os.path.exists(held[1]):
        STATE.held = None
        send_stats(held[0])


def send_stats(request):
    mode = stats_mode()
    if "exit_on_stats" in MODES and STATE.stats_calls > 1:
        assistant([{"type": "text", "text": "late text"}])
        sys.exit(0)
    if mode == "silent":
        return
    if mode == "error":
        respond(request, success=False, error="stats unavailable")
        return
    if mode == "garbled":
        respond(request, data={"unexpected": True})
        return
    t = STATE.totals
    log("stats_response input=%d" % t["input"])
    respond(
        request,
        data={
            "tokens": {
                "input": t["input"],
                "output": t["output"],
                "reasoning": 7,
                "cacheRead": t["cacheRead"],
                "cacheWrite": t["cacheWrite"],
                "total": sum(t.values()),
            },
            "cost": STATE.cost,
        },
    )


def send_maybe_chunked(frame, chunked):
    if not (chunked and STATE.v2):
        emit(frame)
        return
    raw = json.dumps(frame, ensure_ascii=False).encode("utf-8")
    size = max(1, len(raw) // 3 + 1)
    parts = [raw[i : i + size] for i in range(0, len(raw), size)]
    for index, part in enumerate(parts):
        emit(
            {
                "type": "rpc_chunk",
                "chunkId": "fake-1",
                "index": index,
                "count": len(parts),
                "byteLength": len(raw),
                "data": base64.b64encode(part).decode("ascii"),
            }
        )


def assistant(content, usage=True, final=False, provider=None, model=None, counted=True,
              stop_reason="stop", error_message=None, error_status=None, send=True):
    unpriced = "unpriced" in MODES
    message = {
        "role": "assistant",
        "content": content,
        "provider": provider or os.environ.get("FAKE_OMP_PROVIDER", "openai-codex"),
        "model": model or os.environ.get("FAKE_OMP_MODEL", "gpt-5.6-terra"),
        "stopReason": stop_reason,
    }
    if error_message is not None:
        message["errorMessage"] = error_message
    if error_status is not None:
        message["errorStatus"] = error_status
    if os.environ.get("FAKE_OMP_NO_PROVIDER") == "1":
        del message["provider"]
    if usage:
        message["usage"] = {
            "input": USAGE["input"],
            "output": USAGE["output"],
            "cacheRead": USAGE["cacheRead"],
            "cacheWrite": USAGE["cacheWrite"],
            "totalTokens": sum(USAGE.values()),
            "cost": {"total": 0 if unpriced else COST},
        }
        if counted:
            for key in USAGE:
                STATE.totals[key] += USAGE[key]
            if not unpriced:
                STATE.cost += COST
    STATE.last_assistant = message
    if send:
        frame = {"type": "message_end", "message": message}
        if STATE.filter is None or "message_end" in STATE.filter:
            send_maybe_chunked(frame, final and "chunked" in MODES)
    return message


_BUF = b""
TIMEOUT = object()


def read_frame(timeout=None):
    """The next frame from stdin: None at EOF, {} for a line that isn't JSON,
    TIMEOUT when `timeout` seconds pass first. Releases held statistics
    replies while it waits."""
    global _BUF
    deadline = None if timeout is None else time.time() + timeout
    while True:
        service()
        index = _BUF.find(b"\n")
        if index >= 0:
            line, _BUF = _BUF[:index], _BUF[index + 1:]
            try:
                return json.loads(line)
            except ValueError:
                return {}
        ready, _, _ = select.select([0], [], [], 0.02)
        if ready:
            chunk = os.read(0, 65536)
            if not chunk:
                log("eof")
                return None
            _BUF += chunk
        elif deadline is not None and time.time() >= deadline:
            return TIMEOUT


def final_text(prompt_frame):
    if "echo" in MODES:
        overlay = None
        config = flag("--config")
        if config and os.path.exists(config):
            with open(config) as handle:
                overlay = handle.read()
        env = {
            key: value
            for key, value in os.environ.items()
            if key.startswith(("OTEL_", "OMP_", "PI_", "ANTHROPIC_", "XDG_", "CLAUDE_"))
        }
        return json.dumps(
            {
                "argv": ARGS,
                "env": env,
                "state_omp_dir_at_start": STATE_OMP_DIR_AT_START,
                "overlay": overlay,
                "overlay_mode": oct(os.stat(config).st_mode & 0o777) if config else None,
                "message": prompt_frame.get("message"),
                "streamingBehavior": prompt_frame.get("streamingBehavior"),
                "cwd": os.getcwd(),
            }
        )
    if "u2028" in MODES:
        return "before after"
    return "echo:{}|{}".format(
        prompt_frame.get("message"), prompt_frame.get("streamingBehavior")
    )


def run_reports():
    reports = json.loads(
        os.environ.get("FAKE_OMP_REPORTS", '[{"outcome": "done", "summary": ""}]')
    )
    for number, arguments in enumerate(reports):
        call_id = "call-report-{}".format(number)
        assistant(
            [
                {
                    "type": "toolCall",
                    "id": call_id,
                    "name": "report_outcome",
                    "arguments": arguments,
                }
            ]
        )
        emit(
            {
                "type": "host_tool_call",
                "id": "htc-{}".format(number),
                "toolCallId": call_id,
                "toolName": "report_outcome",
                "arguments": arguments,
            }
        )
        rejected = False
        while True:
            frame = read_frame()
            if frame is None:
                sys.exit(0)
            if frame.get("type") == "host_tool_result":
                rejected = bool(frame.get("isError"))
                break
            dispatch(frame)
        emit_event(
            {
                "type": "message_end",
                "message": {
                    "role": "toolResult",
                    "toolCallId": call_id,
                    "toolName": "report_outcome",
                    "content": [{"type": "text", "text": "reported"}],
                    "isError": rejected,
                },
            }
        )
        if not rejected:
            return


def config_value(key):
    config = flag("--config")
    if not config or not os.path.exists(config):
        return None
    with open(config) as handle:
        match = re.search(r"^\s*%s:\s*(.+?)\s*$" % key, handle.read(), re.M)
    return match.group(1).strip("'\"") if match else None


def memory_stats(request):
    mode = os.environ.get("FAKE_OMP_MEMORY_STATS", "healthy")
    db = config_value("dbPath") or ""
    bank = config_value("bank") or ""
    if mode == "exit":
        sys.exit(0)
    if mode == "hang":
        return
    if mode == "error":
        respond(request, success=False,
                error="EACCES: permission denied, mkdir '{}'".format(os.path.dirname(db)))
        return
    if mode == "agent":
        respond(request, data={"agentInvoked": True})
        emit({"type": "prompt_result", "id": request.get("id"), "agentInvoked": True,
              "status": "completed", "sessionSettled": True})
        return
    if mode != "no_output":
        if mode == "other_path":
            db = "/elsewhere/mnemopi.db"
        if mode in ("home_relative", "home_relative_other", "home_relative_default"):
            home = os.environ.get("HOME", "")
            if mode == "home_relative_other":
                db = db.replace("/memory/", "/memory/other-", 1)
            if mode == "home_relative_default":
                db = os.path.join(home, ".omp/agent/memories/mnemopi/mnemopi.db")
            if home and db.startswith(home.rstrip("/") + "/"):
                db = "~/" + db[len(home.rstrip("/")) + 1:]
        if mode == "other_bank":
            bank = "choco-1a2b"
        emit({"type": "command_output",
              "text": "# Mnemopi Memory Stats\n\n"
                      "| Bank | Working | Episodic | Triples | Last memory | Database |\n"
                      "| --- | --- | --- | --- | --- | --- |\n"
                      "| {} | 0 | 0 | 0 | - | {} |\n".format(bank, db)})
    respond(request, data={"agentInvoked": False})


def run_prompt(request):
    prompt_log = os.environ.get("FAKE_OMP_PROMPT_LOG")
    if prompt_log:
        with open(prompt_log, "a") as handle:
            handle.write(json.dumps({"message": request.get("message"),
                                     "streamingBehavior": request.get("streamingBehavior")}) + "\n")
    if request.get("message") == "/memory stats":
        memory_stats(request)
        return
    if os.environ.get("FAKE_OMP_SCRIPT"):
        log("prompt_in:%s" % request.get("message"))
        message = request.get("message") or ""
        if message.startswith("/skill:"):
            respond(request, success=False, error="no such skill: nope")
            log("rejected")
            return
        if message.startswith("/help"):
            # A builtin command: no agent run and no `prompt_result`.
            respond(request, data={"agentInvoked": False})
            log("builtin_answered")
            return
        respond(request, data={"agentInvoked": True})
        if not STATE.script_ran:
            STATE.script_ran = True
            STATE.prompt_id = request.get("id")
            run_script()
        return
    if "reject_noerror" in MODES:
        respond(request, success=False)
        return
    if "reject_prompt" in MODES or "reject_prompt_late" in MODES:
        respond(request, success=False, error="no such skill: nope")
        if "reject_prompt_late" in MODES:
            emit({"type": "prompt_result", "id": request.get("id"), "agentInvoked": False,
                  "status": "error", "error": {"message": "late"}, "sessionSettled": True})
        return
    if "local_command" in MODES:
        # A builtin slash command: no agent turn and no `prompt_result`.
        respond(request, data={"agentInvoked": False})
        return
    respond(request, data={"agentInvoked": True})
    emit_event({"type": "agent_start"})
    if "reject_followup" in MODES:
        second = read_frame()
        respond(second, success=False, error="no such skill: nope")
    if "garbage" in MODES:
        sys.stdout.write("this is not json\n")
        emit({"type": "totally_unknown_frame", "n": 1})
        sys.stdout.flush()
    if "stray_result" in MODES:
        emit({"type": "prompt_result", "id": "nobody-sent-this", "agentInvoked": True,
              "status": "completed", "sessionSettled": True})
    if "unknown_tool" in MODES:
        emit({"type": "host_tool_call", "id": "htc-unknown", "toolCallId": "call-unknown",
              "toolName": "mystery", "arguments": {}})
        while True:
            frame = read_frame()
            if frame is None:
                sys.exit(0)
            if frame.get("type") == "host_tool_result":
                assistant([{"type": "text",
                            "text": "unknown-tool-reply:" + json.dumps(frame)}])
                break
            dispatch(frame)
    if "bad_host_call" in MODES:
        emit({"type": "host_tool_call", "id": "htc-bad"})
        while True:
            frame = read_frame()
            if frame is None:
                sys.exit(0)
            if frame.get("type") == "host_tool_result":
                assistant([{"type": "text",
                            "text": "bad-call-reply:" + json.dumps(frame)}])
                break
            dispatch(frame)
    if "bad_chunk" in MODES:
        emit({"type": "rpc_chunk", "chunkId": "bad", "index": 3, "count": 9,
              "byteLength": 5, "data": "AAAA"})
    if "die" in MODES:
        assistant([{"type": "text", "text": "about to die"}])
        sys.exit(3)
    if "no_messages" in MODES:
        STATE.totals["input"] += int(os.environ.get("FAKE_OMP_SIDE_TOKENS", "0"))
        STATE.cost += float(os.environ.get("FAKE_OMP_SIDE_COST", "0"))
        final = assistant([{"type": "text", "text": "no messages"}], send=False, counted=False)
        emit_event({"type": "agent_end", "isTerminal": True, "yielded": True,
                    "awaitingAsyncWork": False, "messages": [final]})
        emit({"type": "prompt_result", "id": request.get("id"), "agentInvoked": True,
              "status": "completed", "sessionSettled": True})
        return
    # A read tool call and its result, to exercise tool correlation.
    assistant(
        [
            {"type": "thinking", "thinking": "thinking it over"},
            {"type": "toolCall", "id": "call-read", "name": "read",
             "arguments": {"path": "a.txt"}},
        ]
    )
    emit_event(
        {
            "type": "message_end",
            "message": {
                "role": "toolResult",
                "toolCallId": "call-read",
                "toolName": "read",
                "content": [{"type": "text", "text": "file "}, {"type": "text", "text": "body"}],
                "isError": False,
            },
        }
    )
    if "noreport" not in MODES:
        run_reports()
    if "discarded" in MODES:
        if os.environ.get("FAKE_OMP_DISCARD_SECOND"):
            assistant([{"type": "text", "text": ""}], counted=False,
                      provider=os.environ.get("FAKE_OMP_SECOND_PROVIDER", "other-provider"),
                      model=os.environ.get("FAKE_OMP_SECOND_MODEL", "other-model"))
        else:
            assistant([{"type": "text", "text": ""}], counted=False)
    if "second_model" in MODES:
        assistant([{"type": "text", "text": "second"}],
                  provider=os.environ.get("FAKE_OMP_SECOND_PROVIDER", "other-provider"),
                  model=os.environ.get("FAKE_OMP_SECOND_MODEL", "other-model"))
    if "side_calls" in MODES:
        STATE.totals["input"] += int(os.environ.get("FAKE_OMP_SIDE_TOKENS", "0"))
    unsettled = "unsettled" in MODES or "unsettled_die" in MODES
    failing = {}
    if "error" in MODES:
        failing = {"stop_reason": "error",
                   "error_message": os.environ.get("FAKE_OMP_ERROR_MESSAGE", "boom")}
        if os.environ.get("FAKE_OMP_ERROR_STATUS"):
            failing["error_status"] = int(os.environ["FAKE_OMP_ERROR_STATUS"])
    final = assistant([{"type": "text", "text": final_text(request)}], final=True, **failing)
    emit_event({"type": "turn_end"})
    emit_event({"type": "agent_end", "isTerminal": not unsettled, "yielded": True,
                "awaitingAsyncWork": unsettled, "messages": [final]})
    if "error" in MODES:
        error = {"message": os.environ.get("FAKE_OMP_ERROR_MESSAGE", "boom"), "retryable": False}
        if os.environ.get("FAKE_OMP_ERROR_STATUS"):
            error["httpStatus"] = int(os.environ["FAKE_OMP_ERROR_STATUS"])
        emit({"type": "prompt_result", "id": request.get("id"), "agentInvoked": True,
              "status": "error", "error": error, "sessionSettled": not unsettled})
        return
    emit({"type": "prompt_result", "id": request.get("id"), "agentInvoked": True,
          "status": "completed", "sessionSettled": not unsettled})
    if "unsettled_die" in MODES:
        sys.exit(0)
    if unsettled:
        # Background work wakes the agent after the yield; it is a run of its
        # own, with its own start and end, before `session_settled`.
        if "reject_unsettled" in MODES:
            second = read_frame()
            respond(second, success=False, error="no such skill: nope")
        emit_event({"type": "agent_start"})
        background = assistant([{"type": "text", "text": "background result"}])
        time.sleep(0.3)
        emit_event({"type": "agent_end", "isTerminal": True, "yielded": True,
                    "awaitingAsyncWork": False, "messages": [background]})
        emit({"type": "session_settled"})


# ---------------------------------------------------------------------------
# Scripted runs
# ---------------------------------------------------------------------------


def gate_path(name):
    return os.path.join(os.environ.get("FAKE_OMP_GATE_DIR", ""), name)


def wait_for(path, what):
    """Blocks, still answering requests, until `path` exists."""
    deadline = time.time() + 60
    while not os.path.exists(path):
        frame = read_frame(timeout=0.05)
        if frame is None:
            sys.exit(0)
        if frame is TIMEOUT:
            if time.time() > deadline:
                log("gate_timeout:%s" % what)
                sys.exit(7)
            continue
        dispatch(frame)


def script_report(args):
    n = STATE.report_n
    STATE.report_n += 1
    call_id = "call-script-%d" % n
    host_id = "htc-script-%d" % n
    assistant([{"type": "toolCall", "id": call_id, "name": "report_outcome",
                "arguments": args}], stop_reason="toolUse")
    emit({"type": "host_tool_call", "id": host_id, "toolCallId": call_id,
          "toolName": "report_outcome", "arguments": args})
    log("host_call:%d" % n)
    while True:
        frame = read_frame()
        if frame is None:
            sys.exit(0)
        if frame.get("type") == "host_tool_result" and frame.get("id") == host_id:
            rejected = bool(frame.get("isError"))
            break
        dispatch(frame)
    log("host_result:%d:%s" % (n, "rejected" if rejected else "accepted"))
    emit_event({"type": "message_end", "message": {
        "role": "toolResult", "toolCallId": call_id, "toolName": "report_outcome",
        "content": [{"type": "text", "text": "reported"}], "isError": rejected}})


def run_script():
    with open(os.environ["FAKE_OMP_SCRIPT"]) as handle:
        steps = json.load(handle)
    for step in steps:
        op = step["op"]
        log("op:%s%s" % (op, ":" + str(step["label"]) if "label" in step else ""))
        if op == "agent_start":
            emit_event({"type": "agent_start"})
        elif op == "turn_end":
            emit_event({"type": "turn_end"})
        elif op == "session_settled":
            emit({"type": "session_settled"})
        elif op == "log":
            log(step["tag"])
        elif op == "pid":
            log("pid:%d" % os.getpid())
        elif op == "sleep":
            time.sleep(step["seconds"])
        elif op == "exit":
            sys.exit(step.get("code", 0))
        elif op == "gate":
            wait_for(gate_path(step["name"]), step["name"])
        elif op == "hold_stats":
            STATE.hold_next = gate_path(step["gate"])
        elif op == "add_totals":
            STATE.totals["input"] += int(step.get("input", 0))
            STATE.cost += float(step.get("cost", 0))
        elif op == "assistant":
            assistant(
                [{"type": "text", "text": step.get("text", "text")}],
                usage=step.get("usage", True),
                counted=step.get("counted", True),
                stop_reason=step.get("stop_reason", "stop"),
                error_message=step.get("error_message"),
                error_status=step.get("error_status"),
            )
        elif op == "report":
            script_report(step["args"])
        elif op == "prompt_result":
            frame = {"type": "prompt_result", "id": STATE.prompt_id, "agentInvoked": True,
                     "status": step.get("status", "completed"),
                     "sessionSettled": step.get("settled", True)}
            if step.get("status") == "error":
                error = {"message": step.get("error_message", "boom"), "retryable": False}
                if step.get("error_status"):
                    error["httpStatus"] = step["error_status"]
                frame["error"] = error
            emit(frame)
        elif op == "agent_end":
            frame = {"type": "agent_end"}
            for key, field, default in (("yielded", "yielded", True),
                                        ("terminal", "isTerminal", True),
                                        ("awaiting", "awaitingAsyncWork", False)):
                value = step.get(key, default)
                if value != "ABSENT":
                    frame[field] = value
            mode = step.get("messages", "last")
            last = STATE.last_assistant
            if mode == "last":
                frame["messages"] = [last] if last else []
            elif mode == "empty":
                frame["messages"] = []
            elif mode == "string":
                frame["messages"] = "oops"
            elif mode == "unknown_reason":
                frame["messages"] = [dict(last, stopReason="mystery")]
            emit_event(frame)
        else:
            raise SystemExit("unknown script op %r" % op)
    log("script_done")


def dispatch(request):
    kind = request.get("type")
    if kind == "negotiate_protocol":
        if "v1_only" in MODES:
            sys.exit(9)
        if "negotiate_error" in MODES:
            respond(request, success=False, error="no v2 today")
            return
        STATE.v2 = request.get("protocolVersion") == 2
        respond(request, data={"protocolVersion": 2})
    elif kind == "set_event_filter":
        STATE.filter = set(request.get("events") or [])
        respond(request, data={"events": request.get("events"),
                               "messageUpdates": request.get("messageUpdates")})
    elif kind == "set_host_tools":
        if "tools_error" in MODES:
            respond(request, success=False, error="cannot register tools")
            return
        names = [tool["name"] for tool in request.get("tools", [])]
        respond(request, data={"toolNames": names})
    elif kind == "get_state":
        if "state_error" in MODES:
            respond(request, success=False, error="state unavailable")
            return
        respond(
            request,
            data={
                "model": {
                    **(
                        {}
                        if os.environ.get("FAKE_OMP_NO_PROVIDER") == "1"
                        else {"provider": os.environ.get("FAKE_OMP_PROVIDER", "openai-codex")}
                    ),
                    "id": os.environ.get("FAKE_OMP_MODEL", "gpt-5.6-terra"),
                },
                "thinkingLevel": "medium",
                "sessionId": STATE.session_id,
                "systemPrompt": ["fake prompt"],
                "dumpTools": [{"name": name} for name in STATE.tools + ["report_outcome"]],
            },
        )
    elif kind == "get_session_stats":
        handle_stats(request)
    elif kind == "prompt":
        run_prompt(request)
    else:
        respond(request, success=False, error="unknown command {}".format(kind))


def main():
    if "--version" in ARGS:
        if os.environ.get("FAKE_OMP_VERSION_FAIL"):
            sys.exit(1)
        print("omp/fake-1.0")
        return
    if "no_ready" in MODES:
        time.sleep(600)
        return
    emit({"type": "ready", "protocolVersion": 1,
          "supportedProtocolVersions": [1] if "v1_only" in MODES else [1, 2],
          "maxFrameBytes": 1048576, "maxReassembledFrameBytes": 67108864})
    while True:
        frame = read_frame()
        if frame is None:
            return
        dispatch(frame)


if __name__ == "__main__":
    main()
