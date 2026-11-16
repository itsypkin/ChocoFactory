Review the coder's diff for the task below and report your verdict as
instructed. Do steps 1–3 before you use the task text as a checklist.

<task title="{{ task.title }}">
{{ stages.spec_check.summary }}
</task>

This may be a re-review: the change can come back here after an earlier
review, a failed CI run, or a human's review on the pull request.

Your own previous report on this task, between the markers below — empty
on a first review:

<previous_review>
{{ stages.internal_review.summary }}
</previous_review>

A human's latest answer to a blocker the coder stopped on, between the markers below. It is empty if
the coder never stopped. It is the human's decision on this task: where it conflicts with the task text
above, the answer wins, and work that follows it is not a defect. Check that the work does follow it.

<blocker_answer>
{{ stages.blocker }}
</blocker_answer>

Before step 1, also read the branch's full commit messages and, if a pull
request for this branch already exists, its comments
(`gh pr view --json comments,reviews`) — a human's findings live there,
not above. If earlier findings exist, from either source:

1. For each one, report resolved / partial / not resolved / regressed /
   withdrawn, with the file and line that settles it. Judge the code
   against the defect the finding describes. Anything short of resolved
   is still a finding: repeat it under Findings, blocking if it blocked
   before or if step 3 of your instructions says it blocks now. The only
   ways to lower its weight are the two step 3 gives: show from the code
   that no test can reach the branch, or show that the branch only chooses
   message text that wouldn't lead the operator to a wrong action. Step 3
   still keeps such a branch blocking when that message is what the task
   is mainly about. A false claim in the coder's work report is weighed by the rule on it
   below, not by step 3, and that holds for an earlier finding about the
   report too, whatever weight the earlier report gave it. Minor findings
   carried unchanged collapse into one line, as step 5 of your instructions
   says, except inaccuracies in the work report, which stay listed in full.
   A finding that turns out not to have
   been a defect is "withdrawn" and moves to Dismissed with its
   "Mitigated by:" fact.
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
4. If a blocking finding is "new code" in a fix for an earlier finding,
   step back before you report and say whether the approach is the
   problem. When each fix opens a new edge case, a rule is usually
   missing. If one is, name that rule in one sentence, and the one test
   that would hold it for every case, in that finding. If the approach is
   sound, say so in one line.

Put the statuses from item 1 under "Prior findings", the section that
comes before "Reviewed" in your summary.

The coder also writes a work report, which the stage that writes the pull
request's description reads. Read it:
`cat "$(cd "$(git rev-parse --git-dir)" && pwd)/choco-work-report.md"`.
You judge the implementation, not the description: don't judge a description
file or the published description, and no rule about headings, length or
style applies to the report. Check each claim in the report against the code
and the command results, and weigh what you find:

- A false claim that hides something missing from the implementation is a
  blocking finding, and you request changes, because the work isn't done.
  For example, the report says a required test, probe or break exists or was
  made, and it doesn't or wasn't. Acknowledging omitted required work in the
  report does not finish it.
- Any other inaccuracy in the report is a minor finding, and by itself it is
  never a reason for `changes_requested`. For example, a claim that
  overstates or misdescribes work that does exist, a wrong file name, a
  stale detail. List each one under Findings, marked minor, with what is
  actually true, so the stage that writes the description can correct the
  claim or leave it out. A minor report inaccuracy stays minor when another
  finding blocks.
- List report inaccuracies in full in every report, never collapsed into a
  "carried minors" line: the stage that writes the description reads only
  your latest report.
- A missing or empty report is not by itself a reason to reject: judge the
  implementation without it, and note the absence as a minor finding when it
  hides no missing required work.

This pull request closes exactly these issues: {{ task.closes }} (if nothing follows the colon, it closes none). The publishing stage writes one `Closes #N` line for each and nothing else can close an issue: a `#N` in the task title only links (`Refs #N`), and every closing keyword followed by an issue reference in the written description or in a review report is rewritten on publishing so it can't close anything (`Fixes #126` becomes `Fixes issue 126`). Never report an issue as resolved, closed or fixed by this pull request unless it is on that list; for each issue on it, judge whether the change does what the issue asks. A work-report claim that an issue closes, or is fixed by this pull request, when that issue is not on the list is an ordinary minor inaccuracy: list it with the correction, so the description doesn't repeat it.
