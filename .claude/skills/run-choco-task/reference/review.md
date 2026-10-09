# Voting rules

How a verdict on a PR is read. The short version, and how to cast one, is in
step 4 of the skill.

- Only comments and reviews newer than the last hand-off count (a review by
  its submission time); before the first hand-off, newer than the head commit.
  A review submitted while the coder revises is carried into the next round,
  and a vote cast then, approval included, is read when the task next reaches
  the gate. Editing an earlier comment to add the marker counts too; editing a
  review after the hand-off does not count, so post a new comment or review.
- Pending and dismissed reviews never vote. The newest vote across comments
  and reviews decides, and a tie resolves to `/request-changes`. A comment
  votes at the later of its creation and last edit, so editing an older marker
  comment makes it the newest vote. Reviews and
  their inline comments are handed to the coder after the comments.
- A marker inside a fenced code block still votes. When you quote the
  convention, indent it or break it up.
- Only `OWNER`, `MEMBER` and `COLLABORATOR` accounts vote (comments and
  reviews), and `[bot]` accounts never do. Anything commenting under your
  account, including an agent, votes as you.
- Prose doesn't retract a verdict, and the first one the poll sees is acted
  on within a minute. To change your mind, post the other marker inside that
  minute.
