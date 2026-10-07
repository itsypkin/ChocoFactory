#!/usr/bin/env python3
"""Stand-in for `claude` that, per user message, emits a sub-agent `result`
line (it carries `parent_tool_use_id`) and then the top-level `result`."""
import json
import sys
import uuid

from fake_common import iter_turns


def emit(obj):
    print(json.dumps(obj), flush=True)


def main():
    session_id = str(uuid.uuid4())
    emit({"type": "system", "subtype": "init", "session_id": session_id})
    for _line in iter_turns(sys.stdin):
        emit(
            {
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "result": "sub",
                "parent_tool_use_id": "toolu_1",
                "total_cost_usd": 0.5,
                "session_id": session_id,
            }
        )
        emit(
            {
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "result": "top",
                "total_cost_usd": 0.01,
                "session_id": session_id,
            }
        )


if __name__ == "__main__":
    main()
