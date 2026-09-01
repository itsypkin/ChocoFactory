# 01 — Idea Honing (raw Q&A log)

This is a running log. Questions and answers are appended in order as they
happen. Not refactored into a summary — the log is the deliverable.

## Q1: Execution model — how does the tool actually run an "agent"?

Does it shell out to an existing agentic CLI (like Claude Code, Codex CLI,
Gemini CLI) as a subprocess and drive it via its CLI/SDK interface, or does
the tool implement its own agent loop directly against model APIs (managing
tool calls, skills, etc. itself)?

**A:** Shell out to existing agentic CLIs (e.g. `claude`, `codex`, `gemini`)
as subprocesses. Keep an abstraction layer over this so we're not locked
into one CLI's interface — we don't yet know if we'll need something more
complex later (e.g. direct API control), so the abstraction should make
that swap possible without reworking the rest of the system.

## Q2: What does a "Project" represent in this tool?

**A:** A Project is a logical grouping entity, not tied to a single repo.
Example: a greenfield initiative spanning 5 repos/modules and 10
microservices is one Project. Or "all operational work" could be another
Project. It groups related work/tasks together. Individual tasks specify
their own working directory/repo. Open follow-up for later: whether a
Project should have shared memory/context across its tasks (deferred, not
decided now).

## Q3: For Type 1 (simple chat/investigation), how do you interact with the agent while a task is running?

**A:** Both/same underlying thread. The same task's conversation can be
viewed and continued live in the UI, or driven asynchronously (e.g. the
agent posts updates while investigating, and you can jump into live chat
in the UI whenever, or interact via CLI).

## Q4: How should the tool keep a task's conversation alive between turns (given both live and async access)?

**A:** Hybrid. While a conversation is actively being used, keep a
long-lived subprocess running (true live chat, no per-turn cold start).
If the conversation goes idle for some period, tear down the process and
persist the CLI's session id to the DB. When the user comes back later,
restore/resume the conversation using that saved session id (spawning a
fresh process resumed into the same session).

## Q5: In Type 3 (coding task), who/what are the "external reviewers" the loop waits for after the PR is opened?

**A:** This should be configurable per project/repo — the reviewer loop is
customizable, not fixed. Default behavior, two stages:

1. Poll for a bounded time (e.g. ~5 min) for AI/bot/linter/integration-test
   reviewers/checks on the PR. If any are red, fix the code and push a new
   revision addressing their feedback; keep looping this stage until green
   (or time-boxed out).
2. Once stage 1 is all green, notify the customer the PR is ready for
   human review, then continue polling for human reviewer feedback until
   the PR is approved (fixing requested changes as they come in, same as
   stage 1's fix loop).

## Q6: For Type 2 (design doc), where does the doc live and how do you leave comments?

**A:** Markdown file in the repo (e.g. `docs/design/<task>.md`), like this
SOP's own `03-design.md`. The UI renders it and lets you select text and
leave inline/section comments (Google-Docs-style suggestions), which get
attached as structured feedback. On its next turn, the agent reads open
comments, edits the doc, and marks them resolved.

## Q7: For Type 3 (coding tasks), how is a task's working copy isolated, especially with concurrent tasks on the same project?

**A:** Dedicated git worktree per task. On task start, the tool creates a
new worktree + branch (e.g. `git worktree add ../<project>-wt-<task-id> -b
task/<task-id>`). The task runs entirely inside that worktree dir, so
concurrent tasks never collide on the same checkout. The worktree is
cleaned up after the task is merged/closed. Tool owns the worktree
lifecycle and disk cleanup.

## Q8: Should the coder role and internal reviewer role be independently configurable (CLI + model), or share one config per task?

**A:** Ship with sensible defaults so the tool is usable out of the box,
but make it customizable per role. Each role (coder, reviewer) should be
independently configurable — including CLI/model choice and, importantly,
the **system prompt** used for that role. System prompt customization for
both the coding agent and the reviewer agent is called out as important.

## Q9: How should the tool notify you of events needing attention?

**A:** None for now — skip notifications in this design; defer to a
later iteration.

## Q10: How should the UI be delivered, given the backend may run on a remote machine?

Follow-up raised: the backend should be runnable on a remote machine (not
just the laptop), accessed via SSH port forwarding. Would Tauri support
that?

Discussion: Tauri's webview can point at an arbitrary URL (not just
bundled assets), so it could load a forwarded `localhost:PORT` too — but
that just wraps a browser tab in native chrome while still requiring the
same SSH tunnel, plus per-OS packaging/updates. A plain browser hitting
`http://localhost:PORT` after `ssh -L PORT:localhost:PORT remote-host`
works today with zero extra tooling.

**A:** Local web app (browser-based), served by the Rust backend. This is
the better fit given the remote-machine requirement — SSH port forwarding
(or later a reverse proxy/tailscale) just works against a plain HTTP
server. No native app shell for now.

## Q11: Should v1 target all 3 workflow types, or build one end-to-end first?

**A:** Phase it. Phase 1 = Projects + Tasks core + Type 1 (simple chat)
end-to-end (validates subprocess mgmt, session resume, SQLite state, CLI
abstraction, UI live-view, agent-facing CLI). Phase 2 = Type 3 (coding
task) — adds worktrees, internal/external reviewer loop, PR polling.
Type 2 (design doc) is explicitly deferred as a later follow-up, not part
of this initial build.

## Q12: Is the agent-facing CLI mainly for agent-to-agent delegation, external automation, or both?

**A:** Both. An agent running inside a task should be able to call the CLI
to spawn/delegate a sub-task and check on it (composition between tasks).
Separately, external scripts/automation (cron, CI, other tooling) should
also be able to invoke the CLI headlessly to kick off tasks and poll
status, independent of any running agent.

## Q13: In Type 3's coder↔internal-reviewer loop, what happens if they don't converge?

**A:** Cap iterations (configurable N). After N rounds without the internal
reviewer approving, the task pauses and flags the human for input instead
of looping forever — the human can redirect, adjust the task, or approve
as-is.

## Q14: Is GitHub the only Git hosting/PR target for v1, or should the design be host-agnostic from the start?

**A:** GitHub only for v1. Build directly against GitHub (`gh` CLI and/or
GitHub API) for PR creation, CI check polling, and review status. Other
hosts (GitLab, etc.) are out of scope until actually needed — no
abstraction layer required now.

## Q15: Does the web UI/API need authentication in v1?

**A:** No auth. Server binds to `127.0.0.1` only by default; reachable
only via SSH port-forwarding (or locally on the same box). No login
screen, no API tokens. The agent-facing CLI on the same box talks to it
directly with no auth needed either.

## Q16: How much of the agent's internal activity should the UI/DB capture — messages only, or full raw event stream?

Discussion: full event stream (tool calls, tool results, thinking blocks)
gives much better visibility/debuggability but has downsides — storage
growth (large diffs/log dumps), needing to normalize each underlying
CLI's own event format into a common schema, sensitive data (secrets,
env dumps, log contents) persisting at rest, and more UI work to render
a timeline vs. a simple chat thread.

**A:** Capture the full raw event stream, normalized into a common event
schema across CLIs, with a **retention policy of 1 year** (events older
than that are pruned) to bound storage growth.

## Q17: Should the tool cap concurrent agent subprocesses across all tasks/projects?

**A:** Unbounded for v1. You're the only user; no global concurrency cap
for now. Can add one later if it becomes a problem.
