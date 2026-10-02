#!/bin/sh
# Usage: release-smoke.sh <archive>   (env: EXPECT_VERSION, optional)
# Installs the archive into a throwaway HOME and starts/stops a daemon from it.
set -eu

[ "$#" -eq 1 ] || { printf 'usage: %s <archive>\n' "$0" >&2; exit 2; }
archive=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
root=$(cd "$(dirname "$0")/.." && pwd)

HOME=$(mktemp -d)
export HOME
bin="$HOME/bin"
started=0

cleanup() {
    if [ "$started" = 1 ]; then
        "$bin/choco" server stop --force >/dev/null 2>&1 || true # best effort on the way out
    fi
    rm -rf "$HOME"
}
fail() {
    printf 'release-smoke: FAIL: %s\n' "$*" >&2
    log="$HOME/.config/chocofactory/logs/chocofactoryd.log"
    if [ -f "$log" ]; then
        printf -- '--- %s ---\n' "$log" >&2
        cat "$log" >&2
    fi
    exit 1
}
trap cleanup EXIT

CHOCO_INSTALL_ARCHIVE=$archive CHOCO_INSTALL_DIR=$bin CHOCO_RELEASES_URL=http://127.0.0.1:9/unused \
    sh "$root/install.sh" || fail 'install.sh failed'
for b in choco chocofactoryd; do
    [ -x "$bin/$b" ] || fail "$bin/$b is missing or not executable"
done
vline=$("$bin/chocofactoryd" --version) || fail 'chocofactoryd --version failed'
printf '%s\n' "$vline"
if [ -n "${EXPECT_VERSION:-}" ]; then
    case "$vline" in
        "chocofactoryd $EXPECT_VERSION "*) ;;
        *) fail "expected version $EXPECT_VERSION, got: $vline" ;;
    esac
fi

started=1
CHOCOFACTORY_CLAUDE_BINARY=/usr/bin/false "$bin/choco" server start --port 0 || fail 'server start failed'
status=$("$bin/choco" --json server status) || fail 'server status failed'
printf '%s\n' "$status"
printf '%s' "$status" | grep -q '"running":true' || fail 'status does not report running: true'
printf '%s' "$status" | grep -q '"choco_binary_found":true' || fail 'daemon did not find its choco binary'
cb=$(printf '%s' "$status" | sed -n 's/.*"choco_binary":"\([^"]*\)".*/\1/p')
[ "$cb" = "$bin/choco" ] || [ "$cb" = "$(cd "$bin" && pwd -P)/choco" ] || fail "choco_binary is '$cb', expected $bin/choco"
"$bin/choco" server stop || fail 'server stop failed'
started=0
printf 'release-smoke: ok\n'
