#!/usr/bin/env python3
"""Stand-in for `omp --mode rpc`.

Speaks enough of omp's JSONL RPC protocol for the adapter: a `ready` frame
offering protocol versions [1, 2], `negotiate_protocol`, `set_event_filter`,
`set_host_tools`, `get_state`, `get_session_stats` and `prompt`. A prompt is
answered with assistant `message_end` frames carrying usage, a
`host_tool_call` for `report_outcome`, a `prompt_result` and a
`session_settled`.

Behaviour is driven by environment variables (tests set them through a tiny
wrapper script, never through the test process's own environment):

- FAKE_OMP_REPORTS: JSON list of `report_outcome` argument objects to call
  in turn, stopping at the first the daemon accepts. Default: one `done`.
- FAKE_OMP_MODES: comma-separated flags, any of
  unpriced   assistant usage has cost.total 0 (a model with no price list)
  error      `prompt_result` is an error (FAKE_OMP_ERROR_MESSAGE, FAKE_OMP_ERROR_STATUS)
  unsettled  `prompt_result.sessionSettled` is false; `session_settled` follows
  garbage    a malformed line and an unknown frame precede the turn
  bad_chunk  an invalid `rpc_chunk` sequence precedes the turn
  chunked    the final assistant message is sent as `rpc_chunk` frames
  die        the process exits mid-turn, before `prompt_result`
  u2028      the final assistant text contains a raw U+2028
  echo       the final assistant text is a JSON report of argv, env and the
             overlay file's contents
  noreport   no `report_outcome` call is made
  side_calls the session statistics include FAKE_OMP_SIDE_TOKENS input
             tokens that no message carries
  no_ready   the process sleeps instead of speaking (never used for success)
  unknown_tool  the turn first calls a host tool the daemon never registered
                and reports the daemon's reply as assistant text
  stray_result  a `prompt_result` for a prompt id nobody sent precedes the turn
  exit_on_stats the process says one more thing and exits when the turn's
                statistics are requested (stdout ends while the daemon waits)
  unsettled_die `prompt_result` says the session is unsettled, then the
                process exits without `session_settled`
  state_error   `get_state` fails
  tools_error   `set_host_tools` fails
  v1_only       `ready` offers only protocol 1; negotiating kills the process
  negotiate_error  `negotiate_protocol` fails (the session stays on v1)
- FAKE_OMP_STATS_SEQ: comma list of `ok|error|garbled|silent`, one per
  `get_session_stats` call (the last repeats). Default `ok`.
- FAKE_OMP_START_TOKENS: input tokens the session already has at start (a
  resumed session).
- FAKE_OMP_PROVIDER / FAKE_OMP_MODEL: what `get_state` reports.
- FAKE_OMP_VERSION_FAIL: `--version` exits 1.
"""
import base64
import json
import os
import sys
import time

ARGS = sys.argv[1:]
MODES = set(filter(None, os.environ.get("FAKE_OMP_MODES", "").split(",")))
USAGE = {"input": 100, "output": 20, "cacheRead": 30, "cacheWrite": 5}
COST = 0.0123


def emit(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


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


STATE = State()


def stats_mode():
    seq = os.environ.get("FAKE_OMP_STATS_SEQ", "ok").split(",")
    index = min(STATE.stats_calls, len(seq) - 1)
    STATE.stats_calls += 1
    return seq[index]


def handle_stats(request):
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


def assistant(content, usage=True, final=False):
    unpriced = "unpriced" in MODES
    message = {
        "role": "assistant",
        "content": content,
        "provider": os.environ.get("FAKE_OMP_PROVIDER", "openai-codex"),
        "model": os.environ.get("FAKE_OMP_MODEL", "gpt-5.6-terra"),
        "stopReason": "stop",
    }
    if usage:
        message["usage"] = {
            "input": USAGE["input"],
            "output": USAGE["output"],
            "cacheRead": USAGE["cacheRead"],
            "cacheWrite": USAGE["cacheWrite"],
            "totalTokens": sum(USAGE.values()),
            "cost": {"total": 0 if unpriced else COST},
        }
        for key in USAGE:
            STATE.totals[key] += USAGE[key]
        if not unpriced:
            STATE.cost += COST
    frame = {"type": "message_end", "message": message}
    send_maybe_chunked(frame, final and "chunked" in MODES)


def read_frame():
    line = sys.stdin.readline()
    if not line:
        return None
    try:
        return json.loads(line)
    except ValueError:
        return {}


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
            if key.startswith(("OTEL_", "OMP_", "PI_", "ANTHROPIC_"))
        }
        return json.dumps(
            {
                "argv": ARGS,
                "env": env,
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
        emit(
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


def run_prompt(request):
    respond(request, data={"agentInvoked": True})
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
    if "bad_chunk" in MODES:
        emit({"type": "rpc_chunk", "chunkId": "bad", "index": 3, "count": 9,
              "byteLength": 5, "data": "AAAA"})
    if "die" in MODES:
        assistant([{"type": "text", "text": "about to die"}])
        sys.exit(3)
    # A read tool call and its result, to exercise tool correlation.
    assistant(
        [
            {"type": "thinking", "thinking": "thinking it over"},
            {"type": "toolCall", "id": "call-read", "name": "read",
             "arguments": {"path": "a.txt"}},
        ]
    )
    emit(
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
    if "side_calls" in MODES:
        STATE.totals["input"] += int(os.environ.get("FAKE_OMP_SIDE_TOKENS", "0"))
    assistant([{"type": "text", "text": final_text(request)}], final=True)
    unsettled = "unsettled" in MODES
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
        time.sleep(0.3)
        emit({"type": "session_settled"})


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
                    "provider": os.environ.get("FAKE_OMP_PROVIDER", "openai-codex"),
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
