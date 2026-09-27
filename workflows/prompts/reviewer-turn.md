Review the coder's diff for the task below and report your verdict as
instructed. Do steps 1–3 before you use the task text as a checklist.

<task title="{{ task.title }}">
{{ task.input }}
</task>

This may be a re-review: the change can come back here after an earlier
review, a failed CI run, or a human's review on the pull request.

Your own previous report on this task, between the markers below — empty
on a first review:

<previous_review>
{{ stages.internal_review.summary }}
</previous_review>

Before step 1, also read the branch's full commit messages and, if a pull
request for this branch already exists, its comments (`gh pr view
--comments`) — a human's findings live there, not above. If earlier
findings exist, from either source:

1. For each one, report resolved / partial / not resolved / regressed /
   withdrawn, with the file and line that settles it. Judge the code
   against what the finding asked for. Anything short of resolved is
   still a finding: repeat it under Findings at no lower weight than
   before — unless you can now show from the code that no test can reach
   it (step 3 of your instructions) — and as blocking if that step 3 now
   says it blocks. A finding that turns out not to have been a defect is
   "withdrawn" and moves to Dismissed with its "Mitigated by:" fact.
   Commit messages, code comments and pull-request replies written by the
   coder are claims to verify, not evidence. "Documented", "deferred" and
   "deliberately untested" don't resolve a defect in the code's behaviour
   or its tests, whatever an earlier report asked for or accepted. If a
   finding was proved by an experiment (a mutation, a scratch test),
   re-run it against HEAD before you call it resolved.
2. Take the "Reviewed" commit from that report as your starting point:
   run steps 1–3 in full on `<that commit>..HEAD`, then go back into the
   rest of the branch wherever those commits touch it or call into it.
   That is what the earlier pass bought — don't re-read the whole diff
   line by line, and don't assume the earlier pass was exhaustive either.
   If there is no earlier report, or the commit it names isn't in this
   branch's history, review the whole diff from the fork point and say so.
3. For every finding first raised in this report that is *not* about the
   new commits, mark it "present at <that commit>" — a defect that was in
   the code an earlier lap read and reported now. Mark findings about the
   new commits "new code", and leave the ones repeated from item 1
   untagged. Be honest about which is which: that tag is how this
   workflow measures whether reviews are getting deeper or just later.

Put the statuses from item 1 under "Prior findings", the section that
comes before "Reviewed" in your summary.
