#!/usr/bin/env python3
"""`claude` stand-in that follows a script, for the turn-completion tests (#90).

The other fixtures each model one fixed turn shape. The rules #90 added
depend on *sequences*: a turn that ends without reporting and reports only
after being nudged, one that never reports however often it's nudged, one
that reports and then keeps running, one whose sub-agent reports instead of
the main agent. So this one reads a list of steps from the JSON file named by
`FAKE_CLAUDE_SCRIPT` (a file rather than the env var itself, set through a
generated wrapper, for the same reason as `fake_claude_reply.py`).

Steps, run in order:

- `{"op": "read_turn"}` — wait for the next user turn on stdin (skipping
  control requests). At EOF the process exits 0, as the real CLI does.
- `{"op": "text", "text": "...", "parent": "toolu_x"?}` — an assistant
  message, from a sub-agent when `parent` is set.
- `{"op": "report", "outcome": "done", "parent": "toolu_x"?, "is_error": false?}`
  — a `report_outcome` tool_use/tool_result pair.
- `{"op": "result", "is_error": false?}` — the CLI's end-of-turn line.
- `{"op": "answer_every_turn", "text": "..."}` — from here on, reply to each
  turn (a nudge, say) with `text` and a `result`, never reporting, until EOF.
- `{"op": "spawn_child", "heartbeat": path, "pid_file": path, "detach": false?}`
  — start a grandchild in this process group that appends to `heartbeat` on
  a loop. It inherits stderr (and so holds the daemon's pipe open) unless
  `detach`, which sends its output nowhere, like a `nohup`ed server.
- `{"op": "init"}` — another `system/init` line, as the real CLI sends at the
  start of each follow-up turn.
- `{"op": "sleep", "seconds": 0.5}` — pause without output.
- `{"op": "emit_forever", "text": "..."}` — ignore stdin and keep emitting
  assistant messages until killed.
- `{"op": "exit"}` — exit 0 immediately, without waiting for stdin EOF.

After the last step the process waits for stdin EOF, then exits 0.
"""
import json
import os
import subprocess
import sys
import time
import uuid

from fake_common import emit, emit_report, read_turn


def assistant_text(session_id, text, parent=None):
    message = {
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": text}]},
        "session_id": session_id,
    }
    if parent is not None:
        message["parent_tool_use_id"] = parent
    emit(message)


def result(session_id, is_error=False):
    emit(
        {
            "type": "result",
            "subtype": "error_during_execution" if is_error else "success",
            "is_error": is_error,
            "result": "",
            "session_id": session_id,
        }
    )


def main():
    args = sys.argv[1:]
    if "--resume" in args:
        session_id = args[args.index("--resume") + 1]
    else:
        session_id = str(uuid.uuid4())

    with open(os.environ["FAKE_CLAUDE_SCRIPT"], encoding="utf-8") as handle:
        steps = json.load(handle)

    emit({"type": "system", "subtype": "init", "session_id": session_id})

    reports = 0
    for step in steps:
        op = step["op"]
        if op == "read_turn":
            if read_turn(sys.stdin) is None:
                return
        elif op == "text":
            assistant_text(session_id, step["text"], step.get("parent"))
        elif op == "report":
            reports += 1
            tool_use_id = "toolu_report_{}".format(reports)
            report_input = {"outcome": step["outcome"], "summary": ""}
            emit_report(
                session_id,
                report_input,
                tool_use_id,
                step.get("parent"),
                step.get("is_error", False),
            )
        elif op == "result":
            result(session_id, step.get("is_error", False))
        elif op == "answer_every_turn":
            while read_turn(sys.stdin) is not None:
                assistant_text(session_id, step["text"])
                result(session_id)
            return
        elif op == "spawn_child":
            child = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    "import sys, time\n"
                    "while True:\n"
                    "    open(sys.argv[1], 'a').write('.')\n"
                    "    time.sleep(0.02)\n",
                    step["heartbeat"],
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL if step.get("detach") else None,
            )
            with open(step["pid_file"], "w") as f:
                f.write(str(child.pid))
        elif op == "init":
            emit({"type": "system", "subtype": "init", "session_id": session_id})
        elif op == "sleep":
            time.sleep(step["seconds"])
        elif op == "emit_forever":
            while True:
                assistant_text(session_id, step["text"])
                time.sleep(0.02)
        elif op == "exit":
            return
        else:
            raise SystemExit("unknown op: {}".format(op))

    while read_turn(sys.stdin) is not None:
        pass


if __name__ == "__main__":
    main()
