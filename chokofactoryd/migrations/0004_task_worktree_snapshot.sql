-- P2-7b (#58): the repo path and project name `worktree::ensure` actually
-- used to create a task's worktree, snapshotted once rather than
-- recomputed on every later lookup. `tasks.config.cwd` (PATCH
-- /tasks/{id}/config, db/tasks.rs::merge_config) and a project's `name`
-- (PATCH /projects/{id}, db/projects.rs::rename) can both change after a
-- worktree-enabled task starts; recomputing the worktree path from their
-- *current* values instead of these would let a later stage derive a path
-- `worktree::ensure` never actually created. NULL for every task whose
-- workflow definition didn't opt into `worktree: true`.
ALTER TABLE tasks ADD COLUMN worktree_repo TEXT;
ALTER TABLE tasks ADD COLUMN worktree_project TEXT;
