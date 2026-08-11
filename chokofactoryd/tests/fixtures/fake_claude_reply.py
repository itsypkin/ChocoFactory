#!/usr/bin/env python3
"""One-shot `claude` stand-in that replies with whatever it is told to.

Like fake_claude_oneshot.py this runs a single turn and exits, but the reply
is read from the file named by FAKE_CLAUDE_REPLY_FILE rather than echoing the
prompt back. That's what lets a test drive an `agent_turn`'s `capture:` (#45):
the reply can be a JSON verdict, deliberately malformed JSON, or anything else
the capture path has to cope with.

The reply comes from a *file* named by an env var, rather than from the env
var itself, because tests set it via a generated wrapper script — process-wide
`set_var` isn't safe to use from tests that run in parallel in one process.

If the file's first line is `BLOCKS`, the remainder is split on a literal `|`
and each part is emitted as its own assistant text block, covering the case
where a turn's text arrives in several blocks the capture has to join back
together.
"""
import json
import os
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

    # Consume the turn so the daemon's write side doesn't see a broken pipe.
    sys.stdin.readline()

    with open(os.environ["FAKE_CLAUDE_REPLY_FILE"], encoding="utf-8") as handle:
        reply = handle.read()

    if reply.startswith("BLOCKS\n"):
        blocks = reply[len("BLOCKS\n") :].split("|")
        reply = "".join(blocks)
    else:
        blocks = [reply]

    emit(
        {
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": b} for b in blocks]},
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
