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
- `{"op": "echo_turn"}` — wait for the next user turn and repeat it back as
  an assistant message, so a test can assert what the daemon actually sent.
- `{"op": "usage_limit", "structured": true?}` — the account hitting its
  usage limit (#92): the limit message, then an error `result`. With
  `structured` (the default) the message also carries the CLI's own
  `error`/`apiErrorStatus` fields; without it, only the text says so.
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
- `{"op": "run", "command": "..."}` — run `command` with `/bin/sh -c` in this
  process's cwd (the task's worktree), failing the process if it fails. Emits
  nothing; lets a test make the "agent" change the worktree.
- `{"op": "raw", "line": {...}}` — emit `line` verbatim as one JSON line, for
  output the other ops don't model (a `background_tasks_changed` list, say).
- `{"op": "exit", "code": 0?}` — exit immediately, without waiting for stdin EOF.
- `{"op": "spawn_escaped", ...}` — start a background job that has left this
  process's group, the way Claude Code's Bash tool runs a command. Options:
  `pid_file` (written by the job once it runs), `setsid` (own session and
  group), `double_fork` (an intermediate process starts the job and exits, so
  the job is reparented), `scrub_env` (the job's environment drops every
  `CHOCOFACTORY_TURN_*` variable), `ignore_sigterm`, `child_pid_file` (a second
  job in the same session, always with a scrubbed environment, writing its own
  pid), `announce` (emit a `background_tasks_changed` line listing the job
  while the intermediate is still alive), `release_file` (after announcing,
  wait for this file to exist), `wait_orphaned` (don't continue until the
  intermediate has exited), `defer_release` (return at once and keep the
  intermediate alive until a later `release_escaped` step). The intermediate
  stays alive until the announce and release are done.
- `{"op": "release_escaped", "release_file": path?}` — wait for `release_file`
  (when given), then let every deferred intermediate go and reap it, so its job
  is orphaned.

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


LIMIT_TEXT = "You've hit your session limit \u00b7 resets 3:40pm (Europe/Berlin)"


def usage_limit(session_id, structured=True):
    """The shape a real turn took when the account ran out of usage (#92).

    `structured` mirrors the fields the CLI recorded in its own transcript
    for that turn; without them only the `result` line's text says what
    happened, which is the case the text-matching fallback covers.
    """
    message = {
        "type": "assistant",
        "message": {
            "model": "<synthetic>",
            "role": "assistant",
            "content": [{"type": "text", "text": LIMIT_TEXT}],
        },
        "session_id": session_id,
    }
    if structured:
        message["error"] = "rate_limit"
        message["isApiErrorMessage"] = True
        message["apiErrorStatus"] = 429
    emit(message)
    emit(
        {
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": True,
            "result": LIMIT_TEXT,
            "session_id": session_id,
        }
    )


JOB_CODE = (
    "import os, signal, sys, time\n"
    "{ignore}"
    "tmp = sys.argv[1] + '.tmp'\n"
    "open(tmp, 'w').write(str(os.getpid()))\n"
    "os.rename(tmp, sys.argv[1])\n"
    "time.sleep(600)\n"
)


def start_job(pid_file, scrub, ignore_sigterm):
    env = dict(os.environ)
    if scrub:
        env = {k: v for k, v in env.items() if not k.startswith("CHOCOFACTORY_TURN_")}
    code = JOB_CODE.format(
        ignore="signal.signal(signal.SIGTERM, signal.SIG_IGN)\n" if ignore_sigterm else ""
    )
    return subprocess.Popen(
        [sys.executable, "-c", code, pid_file],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def wait_for_file(path, seconds=30):
    deadline = time.time() + seconds
    while not os.path.exists(path):
        if time.time() > deadline:
            raise SystemExit("timed out waiting for " + path)
        time.sleep(0.01)


DEFERRED = []


def spawn_escaped(step, session_id):
    read_fd, write_fd = os.pipe()
    intermediate = os.fork()
    if intermediate == 0:
        os.close(write_fd)
        try:
            # Don't hold the daemon's output pipes open.
            null = os.open(os.devnull, os.O_RDWR)
            os.dup2(null, 1)
            os.dup2(null, 2)
            if step.get("setsid"):
                os.setsid()
            start_job(
                step["pid_file"], step.get("scrub_env", False), step.get("ignore_sigterm", False)
            )
            if step.get("child_pid_file"):
                start_job(step["child_pid_file"], True, False)
            if step.get("double_fork") or step.get("wait_orphaned"):
                # Released when the fixture closes its end of the pipe.
                os.read(read_fd, 1)
                os._exit(0)
            time.sleep(600)
        finally:
            os._exit(1)
    os.close(read_fd)
    wait_for_file(step["pid_file"])
    if step.get("child_pid_file"):
        wait_for_file(step["child_pid_file"])
    if step.get("announce"):
        emit(
            {
                "type": "system",
                "subtype": "background_tasks_changed",
                "tasks": [
                    {"task_id": "escaped", "task_type": "local_bash", "description": "escaped job"}
                ],
            }
        )
    if step.get("defer_release"):
        DEFERRED.append((write_fd, intermediate))
        return
    if step.get("release_file"):
        wait_for_file(step["release_file"])
    os.close(write_fd)
    if step.get("double_fork") or step.get("wait_orphaned"):
        os.waitpid(intermediate, 0)


def release_escaped(step):
    if step.get("release_file"):
        wait_for_file(step["release_file"])
    for write_fd, intermediate in DEFERRED:
        os.close(write_fd)
        os.waitpid(intermediate, 0)
    DEFERRED.clear()


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
        elif op == "echo_turn":
            turn = read_turn(sys.stdin)
            if turn is None:
                return
            assistant_text(session_id, "turn text: {}".format(turn))
        elif op == "usage_limit":
            usage_limit(session_id, step.get("structured", True))
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
        elif op == "run":
            subprocess.run(["/bin/sh", "-c", step["command"]], check=True)
        elif op == "raw":
            emit(step["line"])
        elif op == "spawn_escaped":
            spawn_escaped(step, session_id)
        elif op == "release_escaped":
            release_escaped(step)
        elif op == "exit":
            sys.exit(step.get("code", 0))
        else:
            raise SystemExit("unknown op: {}".format(op))

    while read_turn(sys.stdin) is not None:
        pass


if __name__ == "__main__":
    main()
