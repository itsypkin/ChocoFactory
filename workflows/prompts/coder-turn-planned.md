Task: {{ task.title }}
Closes: {{ task.closes }}

The agreed spec is not in this prompt. Before you change anything, read it
with the read-only `get_context` tool, called as
`{"source":"stage","stage":"spec_check","outcome":"ready"}`. The planner's
report is the `summary` inside the answer's `capture`: read the WHOLE report
(Checks, Decisions, Questions and Spec), not only its Spec section, and follow
it in full. If the call fails, answers `unavailable`, or the capture has no
readable summary, you don't have the spec: report `blocked`, name the lookup
that failed and why, and don't guess what the spec said.

Make this change in the current worktree, then commit it.
