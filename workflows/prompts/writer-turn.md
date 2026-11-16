Task: {{ task.title }}
Closes: {{ task.closes }}

Write the description of this branch's pull request, as your instructions say.
You have four inputs.

1. The spec, between the markers below:

<spec>
{{ task.input }}
</spec>

2. The coder's work report. It is in a file in this worktree's private git
   directory. Read it:
   `cat "$(cd "$(git rev-parse --git-dir)" && pwd)/choco-work-report.md"`.
   If the file is missing or empty, write from the other inputs and say in
   the description that the coder left no report; don't stop. If the read
   fails for another reason, say that it could not be read: don't say it
   doesn't exist.

3. The internal reviewer's report, between the markers below. It may note
   inaccuracies in the coder's report; where the two disagree, this one wins.

<internal_review>
{{ stages.internal_review.summary }}
</internal_review>

4. The branch's commits and diff, all of them, not only the last lap's. Find
   the branch's target from what is already there (the branch's open pull
   request if there is one, `git rev-parse --abbrev-ref origin/HEAD`, or the
   repository's default branch), take the merge-base of `HEAD` and the target,
   and read `git log` and `git diff` from that merge-base to `HEAD`. Don't
   fetch, modify or publish anything just to get prose.

Report the description as the `report_outcome` summary, with outcome `done`.
