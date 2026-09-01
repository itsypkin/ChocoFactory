#!/usr/bin/env python3
"""Stand-in for the `claude` CLI, used by ClaudeAdapter's integration tests.

Speaks just enough of `claude --print --output-format=stream-json
--input-format=stream-json [--resume <id>]` to exercise the adapter's
process-spawning and stdin/stdout plumbing without shelling out to the
real CLI (no network, no auth, deterministic).
"""
import json
import sys
import uuid


def main():
    args = sys.argv[1:]
    if "--resume" in args:
        session_id = args[args.index("--resume") + 1]
    else:
        session_id = str(uuid.uuid4())

    emit({"type": "system", "subtype": "init", "session_id": session_id})

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        turn = json.loads(line)
        text = turn["message"]["content"][0]["text"]
        reply = f"echo:{text}"
        emit(
            {
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": reply}]},
                "session_id": session_id,
            }
        )
        emit(
            {
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "result": reply,
                "session_id": session_id,
            }
        )


def emit(obj):
    print(json.dumps(obj), flush=True)


if __name__ == "__main__":
    main()
