Task: {{ task.title }}

<task>
{{ task.input }}
</task>

You are here because the `{{ arrival.from }}` stage ended with the
outcome `{{ arrival.outcome }}`. On the task's first turn both are empty.

- **First turn**: check the task above and report as instructed. The two
  sections below are empty.
- **`spec_questions` → `resumed`**: on an earlier turn you asked a human
  questions, and they have answered. Your earlier report is under "Your
  previous report", and their answer is under "The human's answer". Start
  from the draft spec in your previous report and fold in each answer as a
  decision. Where the answer tells you to decide, decide and give your
  reason. Then run every check again on the result and report as
  instructed. Ask again only about what the answer left open or newly
  opened.

For any other transition (a retry), follow the second entry if "The
human's answer" below has content, and the first entry otherwise.

## Your previous report

{{ stages.spec_check.summary }}

## The human's answer

{{ stages.spec_questions }}
