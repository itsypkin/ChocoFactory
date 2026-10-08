# Voting rules

How a verdict on a PR is read. The short version, and how to cast one, is in
step 4 of the skill.

- Only comments and reviews newer than the head commit count (a review by its
  submission time). Editing an earlier comment to add the marker counts
  too; editing a review counts only while the review is newer than the head
  commit, so after a push post a new comment or review.
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
