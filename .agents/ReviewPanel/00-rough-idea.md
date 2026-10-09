# Rough idea: a review panel (several reviewers on one diff)

Captured verbatim from the owner, 2026-10-09:

> while this wave is working I am thinking of implementing a new feature for choco, currently each step in our state machine is one actor that works synchroniously, like coder --> then reviewer --> done. what if we want to have several reviewer agents looking at the same diff from different points of view for example we have one coder producing the change, and then let's say we have one main reviewer that starts one security alligned reviewer, one architecture focused and one Operational readiness focused reviewer they all have different prompts and looking at the change from different perspective, they report their findings to main reviewer and it makes the decision. I see several ways to implement this #1 prompt reviewer to use subagents (what omc review is doing sometimes) downside: this will be harness dependent (if omp doesn't have subagent support it won't be able to do this) also we won't be able to enforce it #2 adjust our spec in the way that we allow several steps to run in parallel, WDYT?

Follow-up from the owner:

> let's start with writing this idea down, later I would want an architect agent to dive deep into this
