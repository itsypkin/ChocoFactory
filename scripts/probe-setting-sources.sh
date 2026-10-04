#!/bin/sh
# Opt-in probe for #141: does `--setting-sources local` leak the operator's
# `.claude/settings.local.json` into a linked git worktree?
#
# It builds a throwaway repo plus a linked worktree, puts
# {"outputStyle": "Explanatory"} in the MAIN checkout's settings.local.json
# only, then runs the real `claude` from the worktree with
# `--setting-sources project,local` and with `--setting-sources project`,
# and reports the `output_style` from each init line. Passes when `project`
# gives `default`.
#
# This runs the real, billable CLI (two tiny haiku turns). It needs `claude`
# (logged in), `git` and `jq`. Not run by `cargo test`, not part of CI.
set -eu

for tool in claude git jq; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
main="$tmp/main"
wt="$tmp/wt"

git init -q "$main"
git -C "$main" -c user.name=probe -c user.email=probe@example.invalid \
    commit -q --allow-empty -m init
git -C "$main" worktree add -q --detach "$wt"
mkdir -p "$main/.claude"
printf '{"outputStyle": "Explanatory"}\n' > "$main/.claude/settings.local.json"

style_for() {
    # CLAUDECODE is unset for this invocation only: a nested claude may
    # refuse to start inside another Claude Code session.
    (cd "$wt" && env -u CLAUDECODE claude -p "reply with the single word ok" \
        --output-format stream-json --verbose --model haiku \
        --setting-sources "$1" --strict-mcp-config </dev/null) \
        | jq -r 'select(.type == "system" and .subtype == "init") | .output_style' \
        | head -n 1
}

both=$(style_for project,local)
proj=$(style_for project)

printf '%-16s %s\n' "sources" "output_style"
printf '%-16s %s\n' "project,local" "${both:-<none>}"
printf '%-16s %s\n' "project" "${proj:-<none>}"

if [ "$both" != "Explanatory" ]; then
    echo "note: project,local did not show Explanatory; this CLI version may no longer leak through local."
fi
if [ "$proj" = "default" ]; then
    exit 0
fi
echo "FAIL: --setting-sources project gave '${proj:-<none>}', expected 'default'" >&2
exit 1
