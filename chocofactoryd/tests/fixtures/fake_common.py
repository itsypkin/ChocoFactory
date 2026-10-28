"""Shared helpers for the `claude` stand-ins in this directory.

Imported by the fixture scripts (Python puts a script's own directory on
`sys.path`), so every fake speaks the same two parts of the protocol that
changed with #90:

- The adapter may write a stream-json `control_request` (the skills
  allowlist `initialize`) before the first user turn. The real CLI answers it
  and carries on; a fake that tried to read it as a user turn would crash.
- A single-shot turn only completes once the agent calls `report_outcome`.
  The adapter says a turn is single-shot by passing `--append-system-prompt`
  with the outcomes it may report, mirrored in `--mcp-config`'s
  `--outcome` args. `auto_report` stands in for an agent that follows that
  instruction.
"""
import json
import os
import select
import subprocess
import sys

REPORT_OUTCOME_TOOL = "mcp__chocofactory__report_outcome"

# How long the real CLI takes to answer `initialize` when a tool server is
# still connecting (probed on 2.1.310: it answers only after its MCP servers
# have connected or failed). An `mcp_status` that reaches it inside this
# window sees the server as `pending`.
INIT_REPLY_DELAY_SECONDS = 0.3


class _Stdin:
    """An unbuffered line reader over fd 0.

    `select` on the fd then says truthfully whether a line has arrived,
    which a buffered `sys.stdin` can't (it may already hold the next line).
    `pushback` holds lines read early that a later reader must still see.
    """

    def __init__(self):
        self.pushback = []

    def fileno(self):
        return 0

    def readline(self):
        if self.pushback:
            return self.pushback.pop(0)
        data = b""
        while True:
            byte = os.read(0, 1)
            if not byte:
                return data.decode("utf-8", "replace")
            data += byte
            if byte == b"\n":
                return data.decode("utf-8", "replace")


sys.stdin = _Stdin()


def emit(obj):
    print(json.dumps(obj), flush=True)


def flag(args, name):
    """The value following `name`, or "<unset>" if it wasn't passed."""
    if name in args:
        index = args.index(name) + 1
        if index < len(args):
            return args[index]
    return "<unset>"


def is_control_request(line):
    try:
        return json.loads(line).get("type") == "control_request"
    except ValueError:
        return False


def answer_control_request(line):
    """Answers a control request the way the real CLI does (#346).

    - `initialize`: a success reply. The real CLI sends it only after its
      MCP servers have connected or failed.
    - `mcp_status`: depends on `FAKE_CLAUDE_SETTINGS` (set by a wrapper
      script, never the test process's environment):
        `apply` (default): starts the `--mcp-config` server with the
          environment real Claude Code gives it, including the `env` of a
          `--settings` JSON object, and reports what it lists.
        `reject`: the same without the settings `env`, which is what real
          Claude Code does on a settings bundle it rejected. The MCP
          config's own `env` is still applied.
        `silent`: never answers.
        `refuse`: answers with an error, as a CLI without `mcp_status`.
    """
    try:
        request = json.loads(line)
    except ValueError:
        return
    rid = request.get("request_id")
    subtype = (request.get("request") or {}).get("subtype")
    if subtype == "initialize":
        if has_sentinel(sys.argv[1:]):
            wait_for_servers()
        emit(
            {
                "type": "control_response",
                "response": {"subtype": "success", "request_id": rid, "response": {}},
            }
        )
    elif subtype == "mcp_status":
        mode = os.environ.get("FAKE_CLAUDE_SETTINGS", "apply")
        if mode == "silent":
            return
        if mode == "refuse":
            emit(
                {
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": rid,
                        "error": "Unsupported control request subtype: mcp_status",
                    },
                }
            )
            return
        emit(
            {
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": rid,
                    "response": {"mcpServers": mcp_servers(sys.argv[1:], mode == "apply")},
                },
            }
        )


def has_sentinel(args):
    """Whether the `--mcp-config` server was given a settings sentinel."""
    return "--claude-settings-sentinel" in flag(args, "--mcp-config")


def wait_for_servers():
    """Models the real CLI answering `initialize` only once its servers are up.

    An `mcp_status` request that arrives before that answer is replied to
    with the tool server still `pending`, as the real CLI does (#346). Any
    other line read early is pushed back for the turn reader.
    """
    ready, _, _ = select.select([0], [], [], INIT_REPLY_DELAY_SECONDS)
    if not ready:
        return
    line = sys.stdin.readline()
    try:
        request = json.loads(line)
    except ValueError:
        request = {}
    if (request.get("request") or {}).get("subtype") == "mcp_status":
        emit(
            {
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request.get("request_id"),
                    "response": {
                        "mcpServers": [{"name": "chocofactory", "status": "pending"}]
                    },
                },
            }
        )
    else:
        sys.stdin.pushback.append(line)


def mcp_servers(args, apply_settings):
    """Starts each `--mcp-config` server and reports it like `mcp_status`."""
    try:
        config = json.loads(flag(args, "--mcp-config"))
    except ValueError:
        return []
    settings_env = {}
    if apply_settings:
        try:
            settings = json.loads(flag(args, "--settings"))
        except ValueError:
            settings = None
        if isinstance(settings, dict) and isinstance(settings.get("env"), dict):
            settings_env = {k: str(v) for k, v in settings["env"].items()}
    servers = []
    for name, server in config.get("mcpServers", {}).items():
        env = dict(os.environ)
        env.update(server.get("env", {}))
        env.update(settings_env)
        entry = {
            "name": name,
            "config": {
                "type": "stdio",
                "command": server["command"],
                "args": server.get("args", []),
            },
        }
        tools = probe_server([server["command"]] + server.get("args", []), env)
        if tools is None:
            entry["status"] = "failed"
            entry["error"] = "Connection closed"
        else:
            entry["status"] = "connected"
            entry["tools"] = [{"name": t} for t in tools]
        servers.append(entry)
    return servers


def probe_server(argv, env):
    """The tool names `argv` (an MCP stdio server) lists, or None if it fails."""
    try:
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            bufsize=0,
        )
    except OSError:
        return None
    try:
        lines = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        ]
        try:
            for item in lines:
                proc.stdin.write((json.dumps(item) + "\n").encode())
            proc.stdin.flush()
        except (BrokenPipeError, OSError):
            return None
        # Read the fd directly: select() on a buffered text stream misses
        # lines already sitting in its buffer, and would wait for a reply
        # that has in fact arrived.
        fd = proc.stdout.fileno()
        buffered = b""
        while True:
            while b"\n" in buffered:
                raw, buffered = buffered.split(b"\n", 1)
                try:
                    reply = json.loads(raw)
                except ValueError:
                    continue
                if reply.get("id") == 2:
                    return [t["name"] for t in reply.get("result", {}).get("tools", [])]
            ready, _, _ = select.select([fd], [], [], 60)
            if not ready:
                return None
            chunk = os.read(fd, 65536)
            if not chunk:
                return None
            buffered += chunk
    finally:
        try:
            proc.stdin.close()
        except OSError:
            pass
        proc.kill()
        proc.wait()


_started = False


def record_startup():
    """Writes the argv to `$FAKE_CLAUDE_ARGV_FILE`, once, if that is set."""
    global _started
    if _started:
        return
    _started = True
    path = os.environ.get("FAKE_CLAUDE_ARGV_FILE")
    if path:
        with open(path, "w") as f:
            json.dump(sys.argv[1:], f)


def touch_prompt_marker():
    path = os.environ.get("FAKE_CLAUDE_PROMPT_MARKER")
    if path:
        open(path, "a").close()


def read_turn(stream):
    """The next user-turn line from `stream`, answering control requests.

    Returns None at EOF.
    """
    record_startup()
    while True:
        line = stream.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            continue
        if is_control_request(line):
            answer_control_request(line)
            continue
        touch_prompt_marker()
        return line


def iter_turns(stream):
    while True:
        line = read_turn(stream)
        if line is None:
            return
        yield line


def allowed_outcomes(args):
    """The `--outcome` values in the adapter's `--mcp-config` server args."""
    raw = flag(args, "--mcp-config")
    if raw == "<unset>":
        return []
    try:
        config = json.loads(raw)
    except ValueError:
        return []
    outcomes = []
    for server in config.get("mcpServers", {}).values():
        server_args = server.get("args", [])
        for i, arg in enumerate(server_args):
            if arg == "--outcome" and i + 1 < len(server_args):
                outcomes.append(server_args[i + 1])
    return outcomes


def is_single_shot(args):
    return "--append-system-prompt" in args


def emit_report(
    session_id, report_input, tool_use_id="toolu_report", parent=None, is_error=False
):
    """One `report_outcome` tool_use/tool_result pair.

    `is_error` makes the result a rejection, as the real tool answers an
    outcome the stage doesn't allow.
    """
    call = {
        "type": "assistant",
        "message": {
            "content": [
                {
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": REPORT_OUTCOME_TOOL,
                    "input": report_input,
                }
            ]
        },
        "session_id": session_id,
    }
    result = {
        "type": "user",
        "message": {
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": "outcome not allowed"
                    if is_error
                    else "Recorded outcome '{}'.".format(report_input.get("outcome")),
                    "is_error": is_error,
                }
            ]
        },
        "session_id": session_id,
    }
    if parent is not None:
        call["parent_tool_use_id"] = parent
        result["parent_tool_use_id"] = parent
    emit(call)
    emit(result)


def auto_report(args, session_id):
    """Report completion the way a compliant agent would, if single-shot.

    Reports `done` when the stage allows it, otherwise its first allowed
    outcome. A no-op for a standing session (chat), which is never told to
    report.
    """
    if not is_single_shot(args):
        return
    outcomes = allowed_outcomes(args)
    outcome = "done" if (not outcomes or "done" in outcomes) else outcomes[0]
    emit_report(session_id, {"outcome": outcome, "summary": ""})
