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

REPORT_OUTCOME_TOOL = "mcp__chocofactory__report_outcome"


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


def read_turn(stream):
    """The next user-turn line from `stream`, skipping control requests.

    Returns None at EOF.
    """
    while True:
        line = stream.readline()
        if not line:
            return None
        line = line.strip()
        if not line or is_control_request(line):
            continue
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
