#!/usr/bin/env python3
"""Stand-in for a `claude` invocation that crashes after starting up.

Used to test that drain_session records an abnormal subprocess exit as
`exited` rather than `idle` — a deterministic failure resumed as `idle`
would just loop back into the same crash forever.
"""
import json
import sys
import uuid


def emit(obj):
    print(json.dumps(obj), flush=True)


def main():
    session_id = str(uuid.uuid4())
    emit({"type": "system", "subtype": "init", "session_id": session_id})
    sys.exit(1)


if __name__ == "__main__":
    main()
