#!/bin/bash
# Paired REAL-model probe: real Claude (timeout-backgrounded leftover) + real omp (named service, autonomous wake)
# in one two-branch review group, on a private throwaway daemon. One attempt; hard 8-minute bound.
set -u
REPO_ROOT="${REPO_ROOT:?set REPO_ROOT to a checkout whose target/debug has choco and chocofactoryd built}"
CHOCO="$REPO_ROOT/target/debug/choco"
OPHOME="$HOME"
REAL_CLAUDE="$(readlink -f "$(command -v claude)" 2>/dev/null || command -v claude)"
REAL_OMP="$(readlink -f "$(command -v omp)" 2>/dev/null || command -v omp)"
P="${TMPDIR%/}/choco paired $$"
HOMEDIR="$P/home"; PROBE="$P/probe"
LOCK="$HOMEDIR/.config/chocofactory/chocofactoryd.lock"
DB="$HOMEDIR/.config/chocofactory/chocofactory.db"
mkdir -p "$HOMEDIR" "$P/launch" "$P/flow" "$PROBE" "$P/repo dir"
log() { echo "[$(date -u +%H:%M:%S)] $*"; }
sq() { sqlite3 -readonly "$DB" "$1" 2>&1; }
ch() {
  [ -e "$LOCK" ] || { log "ABORT: private lock missing; not falling back to the shared daemon"; exit 9; }
  env -u CHOCO_BASE_URL HOME="$HOMEDIR" PATH="$P/launch:$PATH" "$CHOCO" "$@"
}
TASK=""
cleanup() {
  log "cleanup"
  if [ -e "$LOCK" ]; then
    [ -n "$TASK" ] && { ch task cancel "$TASK" >/dev/null 2>&1; sleep 2; }
    env -u CHOCO_BASE_URL HOME="$HOMEDIR" "$CHOCO" server stop --force; log "server stop exit $?"
  fi
  for f in "$PROBE/claude_job.pid" "$PROBE/omp_service.pid"; do [ -s "$f" ] && kill "$(cat "$f")" 2>/dev/null; done
  sleep 1
  env -u CHOCO_BASE_URL HOME="$HOMEDIR" "$CHOCO" server status >/dev/null 2>&1; log "server status after stop exit $? (3 = not running)"
  ps -axo pid,ppid,pgid,stat,lstart,command | grep -F "$P" | grep -v grep && log "STILL RUNNING under $P" || log "nothing running under probe dir"
  rm -rf "$P"
}
trap cleanup EXIT

# launchers: restore the operator HOME for the real CLI only; every argument and variable otherwise untouched
printf '#!/bin/sh\nHOME="%s" exec "%s" "$@"\n' "$OPHOME" "$REAL_CLAUDE" > "$P/launch/claude"
printf '#!/bin/sh\nHOME="%s" exec "%s" "$@"\n' "$OPHOME" "$REAL_OMP" > "$P/launch/omp"
chmod +x "$P/launch/claude" "$P/launch/omp"

cat > "$PROBE/claude_job.py" <<'PY'
import os, sys, time
d = os.path.dirname(os.path.abspath(__file__))
open(os.path.join(d, "claude_job.pid"), "w").write(str(os.getpid()))
open(os.path.join(d, "claude_job.ready"), "w").write("ready")
end = time.time() + 120
while time.time() < end:
    open(os.path.join(d, "claude_job.heartbeat"), "a").write(".")
    time.sleep(1)
PY
cat > "$PROBE/omp_service.py" <<'PY'
import os, time
d = os.path.dirname(os.path.abspath(__file__))
open(os.path.join(d, "omp_service.pid"), "w").write(str(os.getpid()))
end = time.time() + 170
while time.time() < end and not os.path.exists(os.path.join(d, "release")):
    time.sleep(0.2)
print("RELEASE-TOKEN: " + open(os.path.join(d, "token.txt")).read().strip(), flush=True)
PY
cat > "$P/flow/claude.md" <<MD
You are a mechanical probe. Do exactly these steps and nothing else.
1. Make ONE foreground Bash tool call. Command: python3 "$PROBE/claude_job.py" . Set the Bash tool's timeout parameter to 8000 (milliseconds). Do NOT set run_in_background, and do not use nohup, &, setsid, disown or any service manager.
2. The tool result will say the command timed out and was moved to the background. Do not wait for it, poll it, read its output or kill it.
3. Immediately call the report_outcome tool with outcome "changes_requested" and summary "claude-probe-5c1d", then end your turn with the single word: done.
MD
cat > "$P/flow/omp.md" <<MD
You are a mechanical probe. Follow these steps exactly.
1. Make ONE bash tool call that starts a service: name "probe-service", pty false, command: python3 "$PROBE/omp_service.py" . Do not set async, ready or any other option.
2. Do NOT poll, check, wait, sleep, read its output or call any other tool. Do NOT call report_outcome yet. Immediately end your turn with the single word: started.
3. Later you will be woken automatically with the service's completion output, which contains a line "RELEASE-TOKEN: <value>". Only then call report_outcome with outcome "changes_requested" and summary "omp-probe <value>" using exactly that value, and end your turn with the single word: done.
MD
cat > "$P/flow/paired.yaml" <<'YAML'
name: paired-probe
worktree: true
roles:
  claude_reviewer:
    cli: claude
    model: claude-sonnet-5-5:low
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
  omp_reviewer:
    cli: omp
    model: openai-codex/gpt-6.1-sol:low
    read_only: true
    disallowed_tools: [edit, write, notebook_edit]
stages:
  panel:
    kind: parallel
    branches:
      claude_branch: { kind: agent_turn, role: claude_reviewer, prompt_file: claude.md, capture: json, results: [changes_requested] }
      omp_branch: { kind: agent_turn, role: omp_reviewer, prompt_file: omp.md, capture: json, results: [changes_requested] }
    on: { done: finished }
  finished:
    kind: terminal
YAML
( cd "$P/repo dir" && git init -q && git -c user.email=a@b -c user.name=t commit -q --allow-empty -m init )

log "versions: $("$REAL_CLAUDE" --version) | $("$REAL_OMP" --version) | choco $("$CHOCO" --version)"
log "claude=$REAL_CLAUDE omp=$REAL_OMP (launchers restore HOME for the real CLIs only; CHOCOFACTORY_CLAUDE_BINARY unset)"
env -u CHOCO_BASE_URL -u CHOCOFACTORY_CLAUDE_BINARY HOME="$HOMEDIR" PATH="$P/launch:$PATH" "$CHOCO" server start --port 0
log "server start exit $?"
ch project create paired --repo "$P/repo dir" --json >/dev/null || exit 1
START=$SECONDS
TASK=$(ch task create --project paired --workflow "$P/flow/paired.yaml" --title "paired probe" --prompt "go" --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
log "task $TASK created"
RELEASED=0; OMPYIELD=""; CLAUDEDONE=""
while [ $((SECONDS-START)) -lt 460 ]; do
  ST=$(sq "select status from tasks where id='$TASK'")
  case "$ST" in closed|stuck|cancelled) log "task settled: $ST"; break;; esac
  if [ -z "$OMPYIELD" ]; then
    N=$(sq "select count(*) from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' and s.stage='omp_branch' and e.event_type='turn_completed'")
    [ "${N:-0}" -ge 1 ] 2>/dev/null && { OMPYIELD=$(date -u +%H:%M:%S); log "omp first (report-less) end seen"; }
  fi
  if [ -z "$CLAUDEDONE" ]; then
    S=$(sq "select json_extract(payload,'\$.parallel.branches.claude_branch.state') from workflow_state where task_id='$TASK'")
    [ "$S" = "done" ] && { CLAUDEDONE=$(date -u +%H:%M:%S); log "claude branch finalized done"; }
  fi
  if [ "$RELEASED" = 0 ] && [ -n "$OMPYIELD" ] && [ -n "$CLAUDEDONE" ]; then
    TOK="tok-$(python3 -c 'import secrets; print(secrets.token_hex(4))')"
    echo "$TOK" > "$PROBE/token.txt"; touch "$PROBE/release"; RELEASED=1
    log "released the omp service; token $TOK revealed only now"
  fi
  for j in claude_job omp_service; do
    if [ -s "$PROBE/$j.pid" ]; then
      A=$(ps -p "$(cat "$PROBE/$j.pid")" -o pid=,ppid=,pgid=,stat=,lstart= 2>/dev/null)
      [ "$A" != "$(eval echo \${LAST_$j:-})" ] && { log "ps $j: ${A:-gone}"; eval "LAST_$j=\"\$A\""; }
    fi
  done
  sleep 1
done
log "elapsed $((SECONDS-START))s; released=$RELEASED"
sleep 3
log "--- sessions"; sq "select substr(id,1,8),stage,cli_adapter,model,status,end_reason,cleanup,started_at,ended_at from sessions where task_id='$TASK' order by started_at"
log "--- task"; sq "select status||' | '||coalesce(stuck_reason,'') from tasks where id='$TASK'"
log "--- payload"; sq "select payload from workflow_state where task_id='$TASK'" | cut -c1-900
log "--- stage entries"; sq "select group_concat(json_extract(payload,'\$.stage')||':'||coalesce(json_extract(payload,'\$.outcome'),'-'),', ') from events where task_id='$TASK' and event_type='stage_entered'"
log "--- turn_outcome"; sq "select created_at, payload from events where task_id='$TASK' and event_type='turn_outcome'"
log "--- session notes / turn ends (kind, time)"; sq "select s.stage, e.event_type, coalesce(json_extract(e.payload,'\$.kind'),''), coalesce(json_extract(e.payload,'\$.is_error'),''), e.created_at from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' and e.event_type in ('session_note','turn_completed') order by e.created_at"
log "--- event type counts per branch"; sq "select s.stage, e.event_type, count(*) from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' group by 1,2 order by 1,2"
log "--- claude tool results (first 300 chars each)"; sq "select e.created_at, substr(e.payload,1,300) from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' and s.stage='claude_branch' and e.event_type in ('tool_call','tool_result') order by e.created_at"
log "--- omp turn_completed payloads"; sq "select e.created_at, substr(e.payload,1,400) from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' and s.stage='omp_branch' and e.event_type='turn_completed' order by e.created_at"
log "--- omp tool calls"; sq "select e.created_at, substr(e.payload,1,300) from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' and s.stage='omp_branch' and e.event_type='tool_call' order by e.created_at"
log "--- cleanup notes"; sq "select s.stage, e.created_at, substr(e.payload,1,500) from events e join sessions s on s.id=e.session_id where s.task_id='$TASK' and e.event_type='session_note' and json_extract(e.payload,'\$.kind') in ('lingered','leftovers_killed','leftovers_unchecked','nudge') order by e.created_at"
log "--- probe files"; ls -la "$PROBE" | sed 's/^/    /'
log "--- processes under probe (independent ps)"; ps -axo pid,ppid,pgid,stat,lstart,command | grep -F "$PROBE" | grep -v grep
