Task: {{ task.title }}

{{ task.input }}

You're revising your earlier work on this task. You are back here because
the `{{ arrival.from }}` stage ended with the outcome `{{ arrival.outcome }}`.
That transition is the reason you're back. Don't work the reason out from
which of the sections further down happen to contain text: they keep
whatever an earlier lap left in them. Find your transition in this list and
follow that entry:

- **`internal_review` → `changes_requested`**: the internal reviewer
  rejected your last commit. Its summary is quoted below under "Internal
  reviewer's summary", and it is current. Address every finding in it.
- **`awaiting_human_review` → `changes_requested`**: a human asked for
  changes on the open PR. Their feedback is on the PR, not in this prompt.
  Run `gh pr view --comments`, and
  `gh api "repos/{owner}/{repo}/pulls/$(gh pr view --json number -q .number)/comments"`
  for inline review comments. Read every comment posted after your last
  commit and address each item it raises, not just the first one. The
  internal reviewer's summary below is **not** this feedback.
- **`checks_polling` → `red`**: a CI check failed on the open PR. Run
  `gh pr checks`, then read the failing jobs' logs
  (`gh run view <run-id> --log-failed`) and fix the cause. The internal
  reviewer's summary below is **not** the reason.
- **`escalate_to_human` → `resumed`**: a human stepped in after the task was
  escalated. Their note is quoted below under "A human's note", and it is
  current. Follow it.

If your transition isn't in this list, check `gh pr checks`,
`gh pr view --comments` and `git log` to work out why you're here before
changing anything.

## Internal reviewer's summary

Current only on the `internal_review` path. On any other path this is left
over from an earlier lap (usually the approval that opened the PR), so
ignore it.

{{ stages.internal_review.summary }}

## A human's note

Current only on the `escalate_to_human` path. On any other path this is
left over from an earlier escalation, so ignore it.

{{ stages.escalate_to_human }}

## When you're done

Commit your revisions, then call `report_outcome` with the outcome `done`
once everything is committed.
