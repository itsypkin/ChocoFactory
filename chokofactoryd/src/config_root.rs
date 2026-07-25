//! Where chokofactory's user-owned state lives on disk (`03-design.md`
//! §2.2, P1-8 LLD §2.6). The tool is distributed as a binary — nothing at
//! runtime may assume a source checkout is present or writable — so
//! everything the daemon reads or writes that isn't the binary itself
//! lives under one root that survives a binary upgrade/reinstall
//! untouched.

use std::io;
use std::path::{Path, PathBuf};

/// `$HOME/.config/chokofactory`, or `None` if `$HOME` isn't set. Callers
/// either fall back to an explicit path (tests) or accept that no default
/// exists (e.g. no global config file to load) rather than failing
/// startup just because `$HOME` is unset.
pub fn config_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("chokofactory"))
}

/// The workflows compiled into the `chokofactoryd` binary — checked into
/// the repo's own `workflows/` directory as the source of truth, embedded
/// at build time. Just `chat` for P1-8; `coding-task` joins this list in
/// P2-7 (#18), prompt files and all.
const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[("chat", include_str!("../../workflows/chat.yaml"))];

/// Writes each of `BUILTIN_WORKFLOWS` into `workflows_dir/<name>.yaml`,
/// but only if that file doesn't already exist — a user's edited copy, or
/// one seeded by a previous release, is never overwritten. Creates
/// `workflows_dir` itself if missing.
///
/// Not folded into `WorkflowEngine::new` — constructors stay side-effect-
/// free, matching how `session.rs`'s idle reaper and
/// `task_runs::recover_stale_active_runs` are already separate steps the
/// daemon's startup sequence calls explicitly, not hidden inside a `new`.
pub fn seed_builtin_workflows(workflows_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(workflows_dir)?;
    for (name, source) in BUILTIN_WORKFLOWS {
        let path = workflows_dir.join(format!("{name}.yaml"));
        if !path.exists() {
            std::fs::write(&path, source)?;
        }
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
                "chokofactoryd-config-root-test-{}",
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
    }

    #[test]
    fn never_overwrites_an_existing_seeded_file() {
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let chat_path = dir.path.join("chat.yaml");
        std::fs::write(&chat_path, "name: my-custom-chat\n").unwrap();

        seed_builtin_workflows(&dir.path).unwrap();

        let contents = std::fs::read_to_string(&chat_path).unwrap();
        assert_eq!(contents, "name: my-custom-chat\n");
    }
}
