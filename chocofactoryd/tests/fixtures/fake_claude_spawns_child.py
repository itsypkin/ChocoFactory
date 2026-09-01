#!/usr/bin/env python3
"""Stand-in for `claude` that spawns a long-running child, for cancel tests (#69).

Every other fixture here is a single leaf process, so killing it proves
nothing about the *group*. A real agent turn's weight is in what it starts
— a `npm test`, a dev server, a build — and a cancel that reaped only the
parent would leave those running in the task's working copy while telling
the operator the task was stopped.

So this one models that shape: it forks a grandchild that touches a
heartbeat file on a loop, reports the grandchild's pid, and then blocks
forever. A test can assert both processes are gone after cancelling, and
that the heartbeat file stops advancing.

Requires `CHOCO_TEST_HEARTBEAT` (a path the grandchild writes) and
`CHOCO_TEST_CHILD_PID` (a path this process writes the grandchild's pid
to). Both are per-test temp paths rather than fixed names so tests running
in parallel can't collide.
"""
import json
import os
import subprocess
import sys
import time
import uuid


def main():
    args = sys.argv[1:]
    if "--resume" in args:
        session_id = args[args.index("--resume") + 1]
    else:
        session_id = str(uuid.uuid4())

    heartbeat = os.environ["CHOCO_TEST_HEARTBEAT"]
    child_pid_path = os.environ["CHOCO_TEST_CHILD_PID"]

    # Inherits this process's group (the adapter put us in a fresh one via
    # `process_group(0)`), which is exactly what makes it reachable by the
    # `killpg` under test and unreachable by a plain `child.kill()`.
    child = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import sys, time\n"
            "while True:\n"
            "    open(sys.argv[1], 'a').write('.')\n"
            "    time.sleep(0.02)\n",
            heartbeat,
        ]
    )

    with open(child_pid_path, "w") as f:
        f.write(str(child.pid))
        f.flush()

    emit({"type": "system", "subtype": "init", "session_id": session_id})
    emit(
        {
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "working"}]},
            "session_id": session_id,
        }
    )

    # Never finishes on its own: the only way out is the signal under test.
    # Deliberately not reading stdin — a cancel has to work on a turn that
    # is mid-flight and ignoring input, which is the case the idle reaper's
    # stdin-close cannot handle.
    while True:
        time.sleep(0.05)


def emit(obj):
    print(json.dumps(obj), flush=True)


if __name__ == "__main__":
    main()
