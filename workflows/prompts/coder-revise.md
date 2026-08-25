Task: {{ task.title }}

{{ task.input }}

You're revising your earlier work on this task. There are a few reasons
you might be back here — check what applies:

Reviewer feedback (empty if this isn't why you're back): {{ stages.internal_review.feedback }}

A note from a human (empty if this isn't why you're back): {{ stages.escalate_to_human }}

If both of those are empty, you're probably back because a CI check failed
or a human requested changes on the open PR itself — run `gh pr checks` and
`gh pr view --comments` (or equivalent) to see why before making further
changes. Commit your revisions when you're done.
