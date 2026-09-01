#!/usr/bin/env python3
"""Stand-in for a `claude` invocation that runs one turn and exits.

Unlike fake_claude.py (which stays alive across turns like a chat
session), this mimics an `agent_turn` stage that runs to completion and
exits on its own (e.g. `coding`/`internal_review`, as opposed to chat's
deliberately-open session) — used to test draining a handle's full event
stream into the events table.
"""
import json
import sys
import uuid


def emit(obj):
    print(json.dumps(obj), flush=True)


def main():
    args = sys.argv[1:]
    if "--resume" in args:
        session_id = args[args.index("--resume") + 1]
    else:
        session_id = str(uuid.uuid4())

    emit({"type": "system", "subtype": "init", "session_id": session_id})

    line = sys.stdin.readline().strip()
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


if __name__ == "__main__":
    main()
