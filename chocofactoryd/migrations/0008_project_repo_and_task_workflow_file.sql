-- Project workflows (#88): a project can carry a repo path, and a task
-- records the exact workflow file it was started from.
--
-- `projects.repo_path` is the repo a task defaults its own `--repo`/
-- `config.cwd` to when none is given, and is where `<repo_path>/
-- .chocofactory/workflows/<name>.yaml` is looked up ahead of the global
-- `~/.config/chocofactory/workflows/<name>.yaml` (§2). NULL means the
-- project has no repo of its own — today's behaviour, and every existing
-- row's value.
--
-- `tasks.workflow_path` is the canonical, absolute path of the workflow
-- file a task's `create_task` call actually resolved and started, and is
-- the authority for which file every later reload of that task's workflow
-- uses — never a fresh name lookup. `tasks.workflow_sha256` is that file's
-- SHA-256 (lowercase hex) at creation time, so a later `choco task status`
-- can say whether it has since changed. Both are NULL for every existing
-- row: a task created before this column existed falls back to resolving
-- `workflow_def` by name against the global workflows directory, exactly
-- as it does today.
ALTER TABLE projects ADD COLUMN repo_path TEXT;
ALTER TABLE tasks ADD COLUMN workflow_path TEXT;
ALTER TABLE tasks ADD COLUMN workflow_sha256 TEXT;
