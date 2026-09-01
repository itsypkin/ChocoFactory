//! Where chocofactory's user-owned state lives on disk (`03-design.md`
//! §2.2, P1-8 LLD §2.6). The tool is distributed as a binary — nothing at
//! runtime may assume a source checkout is present or writable — so
//! everything the daemon reads or writes that isn't the binary itself
//! lives under one root that survives a binary upgrade/reinstall
//! untouched.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `$HOME/.config/chocofactory`, or `None` if `$HOME` isn't set. Callers
/// either fall back to an explicit path (tests) or accept that no default
/// exists (e.g. no global config file to load) rather than failing
/// startup just because `$HOME` is unset.
pub fn config_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("chocofactory"))
}

/// The workflows compiled into the `chocofactoryd` binary — checked into
/// the repo's own `workflows/` directory as the source of truth, embedded
/// at build time. `chat` (P1-8) plus `coding-task` (P2-7, #18).
const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[
    ("chat", include_str!("../../workflows/chat.yaml")),
    (
        "coding-task",
        include_str!("../../workflows/coding-task.yaml"),
    ),
];

/// The prompt files `coding-task.yaml`'s `system_prompt_file`/`prompt_file`
/// fields reference, seeded alongside it into `workflows_dir/prompts/` —
/// same embed-and-seed treatment as the workflow YAML itself (#18), since
/// those fields resolve relative to wherever the seeded copy ends up on
/// disk, not the repo. `chat.yaml` has no prompt files of its own.
const BUILTIN_WORKFLOW_PROMPTS: &[(&str, &str)] = &[
    (
        "coder-system.md",
        include_str!("../../workflows/prompts/coder-system.md"),
    ),
    (
        "coder-turn.md",
        include_str!("../../workflows/prompts/coder-turn.md"),
    ),
    (
        "coder-revise.md",
        include_str!("../../workflows/prompts/coder-revise.md"),
    ),
    (
        "reviewer-system.md",
        include_str!("../../workflows/prompts/reviewer-system.md"),
    ),
    (
        "reviewer-turn.md",
        include_str!("../../workflows/prompts/reviewer-turn.md"),
    ),
];

/// Writes `source` to `path`, but only if it doesn't already exist — a
/// user's edited copy, or one seeded by a previous release, is never
/// overwritten.
///
/// Uses `create_new` (atomic create-or-fail), not a separate `exists()`
/// check followed by `write` — the latter is a check-then-act race: two
/// daemon processes seeding the same directory at once (e.g. started
/// concurrently, or overlapping during a supervisor restart) could both
/// observe "missing" before either writes, defeating the "never
/// overwritten" guarantee this function exists to provide. `create_new`
/// fails atomically if the file already exists, and that specific failure
/// (`AlreadyExists`) is treated as success — the file is present, seeded
/// either by an earlier run or a concurrent one, which is exactly the
/// desired end state either way.
fn seed_one(path: &Path, source: &str) -> io::Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            if let Err(err) = file.write_all(source.as_bytes()) {
                // Otherwise a write failure partway through (e.g. disk
                // full) leaves a truncated file behind, and every future
                // startup's `create_new` would see `AlreadyExists` and
                // treat that corrupt fragment as "already seeded" forever.
                // Best-effort: if the cleanup itself fails, the original
                // write error is still what gets reported.
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(err);
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

/// Writes each of `BUILTIN_WORKFLOWS` into `workflows_dir/<name>.yaml` and
/// each of `BUILTIN_WORKFLOW_PROMPTS` into `workflows_dir/prompts/<name>`,
/// via [`seed_one`] — so neither a workflow definition nor a prompt file is
/// ever overwritten once present. Creates `workflows_dir` (and its
/// `prompts` subdirectory) if missing.
///
/// Not folded into `WorkflowEngine::new` — constructors stay side-effect-
/// free, matching how `session.rs`'s idle reaper and
/// `task_runs::recover_stale_active_runs` are already separate steps the
/// daemon's startup sequence calls explicitly, not hidden inside a `new`.
pub fn seed_builtin_workflows(workflows_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(workflows_dir)?;
    for (name, source) in BUILTIN_WORKFLOWS {
        seed_one(&workflows_dir.join(format!("{name}.yaml")), source)?;
    }

    let prompts_dir = workflows_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)?;
    for (name, source) in BUILTIN_WORKFLOW_PROMPTS {
        seed_one(&prompts_dir.join(name), source)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "chocofactoryd-config-root-test-{}",
                uuid::Uuid::new_v4()
            ));
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn seeds_missing_builtin_workflows_and_creates_the_directory() {
        let dir = TempDir::new();
        assert!(!dir.path.exists());

        seed_builtin_workflows(&dir.path).unwrap();

        let chat_path = dir.path.join("chat.yaml");
        assert!(chat_path.is_file());
        let contents = std::fs::read_to_string(&chat_path).unwrap();
        assert!(contents.contains("name: chat"));

        let coding_task_path = dir.path.join("coding-task.yaml");
        assert!(coding_task_path.is_file());
        let contents = std::fs::read_to_string(&coding_task_path).unwrap();
        assert!(contents.contains("name: coding-task"));

        for name in [
            "coder-system.md",
            "coder-turn.md",
            "coder-revise.md",
            "reviewer-system.md",
            "reviewer-turn.md",
        ] {
            assert!(
                dir.path.join("prompts").join(name).is_file(),
                "expected prompts/{name} to be seeded"
            );
        }
    }

    #[test]
    fn never_overwrites_an_existing_seeded_file() {
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let chat_path = dir.path.join("chat.yaml");
        std::fs::write(&chat_path, "name: my-custom-chat\n").unwrap();
        let coding_task_path = dir.path.join("coding-task.yaml");
        std::fs::write(&coding_task_path, "name: my-custom-coding-task\n").unwrap();
        let prompt_paths: Vec<_> = [
            "coder-system.md",
            "coder-turn.md",
            "coder-revise.md",
            "reviewer-system.md",
            "reviewer-turn.md",
        ]
        .into_iter()
        .map(|name| dir.path.join("prompts").join(name))
        .collect();
        for path in &prompt_paths {
            std::fs::write(path, "my custom prompt\n").unwrap();
        }

        seed_builtin_workflows(&dir.path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&chat_path).unwrap(),
            "name: my-custom-chat\n"
        );
        assert_eq!(
            std::fs::read_to_string(&coding_task_path).unwrap(),
            "name: my-custom-coding-task\n"
        );
        for path in &prompt_paths {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                "my custom prompt\n",
                "expected {path:?} to survive a second seed call"
            );
        }
    }

    /// The embedded `coding-task.yaml` and its prompt files aren't just
    /// present after seeding — they have to actually resolve and validate
    /// together (#18), since `system_prompt_file`/`prompt_file` are
    /// relative to wherever the seeded copy ends up on disk, not the repo.
    #[test]
    fn the_seeded_coding_task_workflow_loads_and_validates() {
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();

        let def = crate::workflow_def::WorkflowDefinition::load(&dir.path.join("coding-task.yaml"))
            .unwrap();
        assert_eq!(def.name, "coding-task");
        assert!(def.worktree);
    }
}
