Task: {{ task.title }}
Closes: {{ task.closes }}

The task text is not in this prompt. Before you change anything, read it with
the read-only `get_context` tool, called as `{"source":"task"}`, and follow it
in full. If the call fails, or the answer says the input is not available
(`input_available` is false), you don't have the task: report `blocked`, name
the call that failed and why, and don't guess what the task was.

Make this change in the current worktree, then commit it.
