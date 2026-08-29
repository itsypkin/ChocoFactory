#!/usr/bin/env python3
"""Stand-in for `claude` that reports the config it was invoked with.

Like fake_claude_oneshot.py it runs a single turn and exits, so an
`agent_turn` stage using it completes and auto-advances. Unlike the other
fixtures, its reply is not an echo of the input but a summary of its own
argv: `model=<--model>|system_prompt=<--system-prompt>|permission_mode=<--permission-mode>`.

That makes the *resolved role config* observable from the events table.
`task_runs` persists a run's `cli_adapter`/`model` columns, but nothing
persists the system prompt, so reading it back off the subprocess's
command line is the only way a test can prove which prompt a given role
actually ran with (P2-6/#17, where two roles must each get their own).
"""
import json
import sys
import uuid


def emit(obj):
    print(json.dumps(obj), flush=True)


def flag(args, name):
    """The value following `name`, or "<unset>" if it wasn't passed."""
    if name in args:
        index = args.index(name) + 1
        if index < len(args):
            return args[index]
    return "<unset>"


def main():
    args = sys.argv[1:]
    if "--resume" in args:
        session_id = args[args.index("--resume") + 1]
    else:
        session_id = str(uuid.uuid4())

    emit({"type": "system", "subtype": "init", "session_id": session_id})

    # Read (and discard) the turn, so the adapter's stdin write completes
    # exactly as it would against the real CLI.
    sys.stdin.readline()

    reply = "model={}|system_prompt={}|permission_mode={}".format(
        flag(args, "--model"),
        flag(args, "--system-prompt"),
        flag(args, "--permission-mode"),
    )

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
