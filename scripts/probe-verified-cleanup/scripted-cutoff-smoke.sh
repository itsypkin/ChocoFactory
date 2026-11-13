#!/bin/bash
# Scripted terminating-cutoff smoke on a private throwaway daemon. MECHANICS ONLY: the CLI is a script, not the real model.
set -u
REPO_ROOT="${REPO_ROOT:?set REPO_ROOT to a checkout whose target/debug has choco and chocofactoryd built}"
CHOCO="$REPO_ROOT/target/debug/choco"
FIX="$REPO_ROOT/chocofactoryd/tests/fixtures"
P="${TMPDIR%/}/choco smoke $$"
HOMEDIR="$P/home"
LOCK="$HOMEDIR/.config/chocofactory/chocofactoryd.lock"
mkdir -p "$HOMEDIR" "$P/bin" "$P/flow" "$P/repo dir"
JOBPID_FILE="$P/job.pid"
log() { echo "[$(date -u +%H:%M:%S)] $*"; }
ch() {
  [ -e "$LOCK" ] || { log "ABORT: private lock $LOCK missing; refusing to fall back to the shared daemon"; exit 9; }
  env -u CHOCO_BASE_URL HOME="$HOMEDIR" "$CHOCO" "$@"
}
cleanup() {
  log "cleanup"
  if [ -e "$LOCK" ]; then env -u CHOCO_BASE_URL HOME="$HOMEDIR" "$CHOCO" server stop --force; log "server stop exit $?"; fi
  [ -s "$JOBPID_FILE" ] && kill "$(cat "$JOBPID_FILE")" 2>/dev/null
  sleep 1
  env -u CHOCO_BASE_URL HOME="$HOMEDIR" "$CHOCO" server status >/dev/null 2>&1; log "server status after stop exit $? (3 = not running)"
  ps -axo pid,ppid,pgid,stat,lstart,command | grep -F "$P" | grep -v grep && log "STILL RUNNING under $P" || log "nothing running under probe dir"
  rm -rf "$P"
}
trap cleanup EXIT

# fixtures: probe-only copies, the job bounded to 180s
cp "$FIX/fake_claude_script.py" "$FIX/fake_common.py" "$P/bin/"
sed -i '' 's/time.sleep(600)/time.sleep(180)/' "$P/bin/fake_claude_script.py"
grep -c "time.sleep(180)" "$P/bin/fake_claude_script.py"
cat > "$P/initial.json" <<JSON
[{"op":"read_turn"},
 {"op":"spawn_escaped","setsid":true,"double_fork":true,"pid_file":"$JOBPID_FILE"},
 {"op":"run","command":"echo edited > smoke-edit.txt"},
 {"op":"report","outcome":"done","summary":"first report"},
 {"op":"usage_limit"},
 {"op":"sleep","seconds":120}]
JSON
cat > "$P/resumed.json" <<JSON
[{"op":"echo_turn"},{"op":"report","outcome":"done","summary":"resumed report"},{"op":"result"}]
JSON
cat > "$P/bin/claude-wrapper.sh" <<SH
#!/bin/sh
D="$P"
echo "\$*" | cut -c1-300 >> "\$D/argv.log"
case " \$* " in *" --resume "*) S="\$D/resumed.json";; *) S="\$D/initial.json";; esac
FAKE_CLAUDE_SCRIPT="\$S" exec /usr/bin/env python3 "\$D/bin/fake_claude_script.py" "\$@"
SH
chmod +x "$P/bin/claude-wrapper.sh"
cat > "$P/flow/smoke.yaml" <<'YAML'
name: smoke-cutoff
roles:
  coder:
    cli: claude
    model: sonnet
stages:
  coding:
    kind: agent_turn
    role: coder
    prompt_file: coder-turn.md
    on: { done: finished }
  finished:
    kind: terminal
YAML
echo "Make the change." > "$P/flow/coder-turn.md"
( cd "$P/repo dir" && git init -q && git -c user.email=a@b -c user.name=t commit -q --allow-empty -m init )

log "starting private daemon (mock claude only in ITS environment)"
env -u CHOCO_BASE_URL HOME="$HOMEDIR" CHOCOFACTORY_CLAUDE_BINARY="$P/bin/claude-wrapper.sh" "$CHOCO" server start --port 0
log "server start exit $?"
ch server status
ch project create smoke --repo "$P/repo dir" --json >/dev/null || exit 1
T0=$(date -u +%H:%M:%S)
TASK=$(ch task create --project smoke --workflow "$P/flow/smoke.yaml" --title "smoke cutoff" --prompt "go" --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
log "task $TASK created"
for i in $(seq 1 200); do [ -s "$JOBPID_FILE" ] && break; sleep 0.1; done
log "leftover job while the CLI is alive (independent ps): $(ps -p "$(cat "$JOBPID_FILE")" -o pid=,ppid=,pgid=,stat=,lstart= 2>&1)"
ch task status "$TASK" --until attention --timeout 4m >/dev/null 2>"$P/until.err"; log "wait exit $? $(cat "$P/until.err")"
DB="$HOMEDIR/.config/chocofactory/chocofactory.db"
sq() { sqlite3 -readonly "$DB" "$1"; }
log "sessions after the cutoff:"; sq "select substr(id,1,8),stage,status,end_reason,cleanup,adapter_session_id,resumed_from,started_at,ended_at from sessions order by started_at"
log "task: $(sq "select status||' | '||coalesce(stuck_reason,'') from tasks where id='$TASK'")"
log "job pid file: $(cat "$JOBPID_FILE") alive? $(ps -p "$(cat "$JOBPID_FILE")" -o pid=,stat= 2>&1)"
log "edit survived: $(cat "$P/repo dir/smoke-edit.txt" 2>&1)"
log "stage entries: $(sq "select group_concat(json_extract(payload,'\$.stage')||':'||coalesce(json_extract(payload,'\$.outcome'),'-'),', ') from events where task_id='$TASK' and event_type='stage_entered'")"
log "retrying with --resume"
ch task retry "$TASK" --resume
log "retry exit $?"
ch task status "$TASK" --until stage:finished --timeout 3m >/dev/null 2>"$P/until2.err"; log "wait exit $? $(cat "$P/until2.err")"
log "sessions after the retry:"; sq "select substr(id,1,8),stage,status,end_reason,cleanup,adapter_session_id,substr(resumed_from,1,8),started_at,ended_at from sessions order by started_at"
log "task: $(sq "select status from tasks where id='$TASK'")"
log "stage entries: $(sq "select group_concat(json_extract(payload,'\$.stage')||':'||coalesce(json_extract(payload,'\$.outcome'),'-'),', ') from events where task_id='$TASK' and event_type='stage_entered'")"
log "edit survived: $(cat "$P/repo dir/smoke-edit.txt" 2>&1)"
log "argv log (--resume lines):"; grep -o -- "--resume [^ ]*" "$P/argv.log"
log "all argv lines: $(wc -l < "$P/argv.log")"
log "start $T0"
