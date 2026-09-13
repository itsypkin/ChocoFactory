Review the coder's diff for the task below and report your verdict as
instructed. Do steps 1–3 before you use the task text as a checklist.

<task title="{{ task.title }}">
{{ task.input }}
</task>

This may be a re-review: the change can come back here after an earlier
review, a failed CI run, or a human's review on the pull request. Before
step 1, read the branch's full commit messages and, if a pull request for
this branch already exists, its comments (`gh pr view --comments`). If
earlier findings exist:

1. For each one, report resolved / partial / not resolved / regressed,
   with the file and line that settles it. Judge the code against what the
   finding asked for. Commit messages and pull-request replies written by
   the coder are claims to verify, not evidence.
2. Run steps 1–3 on the commits since that review first — fixes are new
   code — then on the whole branch.

Put this under "Prior findings" at the top of your summary.
