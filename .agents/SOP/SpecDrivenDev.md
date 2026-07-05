# SOP: Plan-to-Spec Driven Development

This is the standard process for taking a rough project idea from pitch to
an executable implementation plan. Point Claude at this file ("use the SOP
in .agents/SOP.md") to run this process without re-explaining it.

## Folder layout

Each project gets its own folder: `.agents/<project-name>/`

Files inside are numbered by phase so ordering is obvious from the filename
alone:

```
.agents/<project-name>/
  00-rough-idea.md
  01-idea.md
  02-research-<topic-a>.md      (optional, one per topic)
  02-research-<topic-b>.md
  03-design.md
  04-plan.md
```

## Step 0 — Kickoff

User pitches a rough idea. Claude creates `.agents/<project-name>/` in the
repo — the home for all planning docs for the project going forward —
and saves the user's rough idea **verbatim, as given** into
`00-rough-idea.md`. This is a raw capture, not a summary: it preserves
exactly what the user said before any clarification happens, so later
phases can always be traced back to the original pitch.

## Step 1 — Idea honing

Create `01-idea.md`. Claude asks clarifying questions **one at a time**
(not a big batch up front), writing each question and the user's answer
into the doc as we go. It's expected that answering one question surfaces
new questions — just append them to the doc and keep going.

The doc stays a **raw running Q&A log**. Do not refactor it into a
polished requirements summary — the log itself is the deliverable, and it
stays untouched once the step is done.

Goal: disambiguate the idea into a clear, unambiguous set of requirements.

## Step 2 — Research (optional)

Skip if nothing needs it. If some aspect of the idea needs deeper
investigation, Claude proposes a short list of specific topics. Each
researched topic gets its own file: `02-research-<topic>.md`.

## Step 3 — Design

Create `03-design.md` based on the idea doc (and research docs, if any).
Structure:

- High-level overview
- Architecture
- Low-level details, but only where they materially matter

Claude hands this doc to the user for review. Iterate on comments
together until the user explicitly approves the design. Do not move to
Step 4 without approval.

## Step 4 — Implementation plan

Create `04-plan.md` based on the approved design. This is a list of
tasks (noting dependencies between tasks where they exist). Each task
must be well-defined enough that an independent agent could pick it up
and implement it with as few follow-up questions as possible — including
a link back to the relevant section of `03-design.md`.

## Standing conventions

- **File naming:** numbered phase files as shown above.
- **Git commits:** Claude never commits planning docs automatically.
  Only commit when the user explicitly asks, regardless of which phase
  just completed.
- **Approval gates:** the design phase (Step 3) requires explicit user
  approval before generating the implementation plan. Earlier phases
  don't block the same way, but do proceed step-by-step rather than
  skipping ahead.
