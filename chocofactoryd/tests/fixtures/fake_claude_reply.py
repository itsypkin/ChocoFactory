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

Three directives may appear on the file's first line:

- `BLOCKS` — the remainder is split on a literal `|` and each part is emitted
  as its own assistant text block of one message, covering a reply the capture
  has to reassemble.
- `TOOL` — emit a narrating assistant message, a tool_use, a tool_result, and
  only then the reply as a second assistant message. This is what a real
  agent's turn looks like whenever it uses a tool, and it is the case where
  concatenating everything the agent said would corrupt the capture.
- `REPORT <outcome>[,<outcome>...]` (issue #73) — before the final assistant
  message, emit one `mcp__chocofactory__report_outcome` tool_use/tool_result
  pair per comma-separated outcome, in order, each carrying
  `{"outcome": <outcome>, "summary": ""}`. More than one outcome exercises
  "the last call wins" (a model correcting itself, or retrying after the
  tool rejected its first attempt). The tool name is hardcoded here rather
  than imported, since this is a standalone script — it must match
  `chocofactory_core::mcp::qualified_report_outcome_tool_name()`.
"""
import json
import os
import sys
import uuid

REPORT_OUTCOME_TOOL = "mcp__chocofactory__report_outcome"


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

    uses_tool = reply.startswith("TOOL\n")
    if uses_tool:
        reply = reply[len("TOOL\n") :]

    report_outcomes = []
    if reply.startswith("REPORT "):
        first_line, _, reply = reply.partition("\n")
        report_outcomes = first_line[len("REPORT ") :].split(",")

    if reply.startswith("BLOCKS\n"):
        blocks = reply[len("BLOCKS\n") :].split("|")
        reply = "".join(blocks)
    else:
        blocks = [reply]

    if uses_tool:
        # What a real turn looks like the moment an agent reaches for a tool:
        # the answer is the *last* message, not everything that was said.
        emit(
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {"type": "text", "text": "I'll read the diff first."},
                        {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Read",
                            "input": {"path": "a.rs"},
                        },
                    ]
                },
                "session_id": session_id,
            }
        )
        emit(
            {
                "type": "user",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_1",
                            "content": "fn main() {}",
                        }
                    ]
                },
                "session_id": session_id,
            }
        )

    for i, outcome in enumerate(report_outcomes):
        tool_use_id = f"toolu_report_{i}"
        emit(
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": REPORT_OUTCOME_TOOL,
                            "input": {"outcome": outcome, "summary": ""},
                        }
                    ]
                },
                "session_id": session_id,
            }
        )
        emit(
            {
                "type": "user",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": f"Recorded outcome '{outcome}'.",
                        }
                    ]
                },
                "session_id": session_id,
            }
        )

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
