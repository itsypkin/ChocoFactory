#!/usr/bin/env python3
"""Stand-in for `claude` that reports the config it was invoked with.

Like fake_claude_oneshot.py it runs a single turn and exits, so an
`agent_turn` stage using it completes and auto-advances. Unlike the other
fixtures, its reply is not an echo of the input but a summary of its own
argv: `model=<--model>|system_prompt=<--system-prompt>|permission_mode=<--permission-mode>|mcp_config=<--mcp-config>|strict_mcp_config=<present?>|setting_sources=<--setting-sources>|disallowed_tools=<--disallowedTools>|disable_auto_memory=<env>|initialize=<control request>|append_system_prompt=<--append-system-prompt>`.

That makes the *resolved role config* observable from the events table.
`sessions` persists a session's `cli_adapter`/`model` columns, but nothing
persists the system prompt, so reading it back off the subprocess's
command line is the only way a test can prove which prompt a given role
actually ran with (P2-6/#17, where two roles must each get their own).
"""
import json
import os
import sys
import uuid

from fake_common import auto_report, emit, flag, is_control_request


def main():
    args = sys.argv[1:]
    if "--resume" in args:
        session_id = args[args.index("--resume") + 1]
    else:
        session_id = str(uuid.uuid4())

    emit({"type": "system", "subtype": "init", "session_id": session_id})

    # Read (and discard) the turn, so the adapter's stdin write completes
    # exactly as it would against the real CLI. A control request ahead of
    # it (the skills allowlist, #90) is reported rather than discarded.
    initialize = "<unset>"
    line = sys.stdin.readline().strip()
    if is_control_request(line):
        initialize = json.dumps(json.loads(line)["request"], separators=(",", ":"))
        sys.stdin.readline()

    reply = "model={}|system_prompt={}|permission_mode={}|mcp_config={}|strict_mcp_config={}|setting_sources={}|disallowed_tools={}|disable_auto_memory={}|initialize={}|append_system_prompt={}".format(
        flag(args, "--model"),
        flag(args, "--system-prompt"),
        flag(args, "--permission-mode"),
        flag(args, "--mcp-config"),
        "true" if "--strict-mcp-config" in args else "false",
        flag(args, "--setting-sources"),
        flag(args, "--disallowedTools"),
        os.environ.get("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "<unset>"),
        initialize,
        flag(args, "--append-system-prompt"),
    )

    auto_report(args, session_id)

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
