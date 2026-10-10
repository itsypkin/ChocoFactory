//! Where chocofactory's user-owned state lives on disk (`03-design.md`
//! §2.2, P1-8 LLD §2.6). The tool is distributed as a binary — nothing at
//! runtime may assume a source checkout is present or writable — so
//! everything the daemon reads or writes that isn't the binary itself
//! lives under one root that survives a binary upgrade/reinstall
//! untouched.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `$HOME/.config/chocofactory`, or `None` if `$HOME` isn't set. The
/// definition lives in `chocofactory-core` so `choco` shares it.
pub fn config_root() -> Option<PathBuf> {
    chocofactory_core::paths::config_root()
}

/// The workflows compiled into the `chocofactoryd` binary — checked into
/// the repo's own `workflows/` directory as the source of truth, embedded
/// at build time. `chat` (P1-8), `coding-task` (P2-7, #18) and
/// `coding-task-planned`, which is `coding-task` with a spec check in front.
const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[
    ("chat", include_str!("../../workflows/chat.yaml")),
    (
        "coding-task",
        include_str!("../../workflows/coding-task.yaml"),
    ),
    (
        "coding-task-planned",
        include_str!("../../workflows/coding-task-planned.yaml"),
    ),
];

/// The prompt files `coding-task.yaml`'s and `coding-task-planned.yaml`'s
/// `system_prompt_file`/`prompt_file` fields reference, seeded alongside it into `<dir>/prompts/` —
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
    (
        "planner-system.md",
        include_str!("../../workflows/prompts/planner-system.md"),
    ),
    (
        "planner-turn.md",
        include_str!("../../workflows/prompts/planner-turn.md"),
    ),
    (
        "coder-turn-planned.md",
        include_str!("../../workflows/prompts/coder-turn-planned.md"),
    ),
    (
        "coder-revise-planned.md",
        include_str!("../../workflows/prompts/coder-revise-planned.md"),
    ),
    (
        "reviewer-turn-planned.md",
        include_str!("../../workflows/prompts/reviewer-turn-planned.md"),
    ),
];

/// The scripts `coding-task.yaml`'s `script_file:` fields reference (#101),
/// seeded into `<dir>/scripts/` executable. Same embed-and-seed
/// treatment as the prompts.
const BUILTIN_WORKFLOW_SCRIPTS: &[(&str, &str)] = &[
    (
        "open-pr.sh",
        include_str!("../../workflows/scripts/open-pr.sh"),
    ),
    (
        "await-review.sh",
        include_str!("../../workflows/scripts/await-review.sh"),
    ),
    (
        "ci-checks.sh",
        include_str!("../../workflows/scripts/ci-checks.sh"),
    ),
];

/// What [`seed_builtin_workflows`] actually did — which files it wrote for
/// the first time and which were already present (issue #88: `choco project
/// init-workflows` reports this back to the caller; the daemon's own
/// startup call only logs it).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeedReport {
    /// Files that did not exist before this call and were just written.
    pub created: Vec<PathBuf>,
    /// Files that already existed (seeded by a previous run, or edited by a
    /// user) and were left untouched.
    pub existing: Vec<PathBuf>,
}

/// Writes `source` to `path`, but only if it doesn't already exist — a
/// user's edited copy, or one seeded by a previous release, is never
/// overwritten. Returns whether this call is the one that created it.
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
/// desired end state either way (and is reported as `existing`, not
/// `created`, since this call didn't write it).
///
/// `mode` is applied at creation, on the same `create_new` open, so a script
/// is never briefly present without its executable bits. It is never applied
/// to an existing file.
fn seed_one(path: &Path, source: &str, mode: u32) -> io::Result<bool> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
    {
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
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

/// Writes each of `BUILTIN_WORKFLOWS` into `workflows_dir/<name>.yaml` and
/// each of `BUILTIN_WORKFLOW_PROMPTS` into `workflows_dir/prompts/<name>`,
/// and each of `BUILTIN_WORKFLOW_SCRIPTS` (mode 0755) into
/// `workflows_dir/scripts/<name>`,
/// via [`seed_one`] — so neither a workflow definition nor a prompt file is
/// ever overwritten once present. Creates `workflows_dir` (and its
/// `prompts` subdirectory) if missing.
///
/// Not folded into `WorkflowEngine::new` — constructors stay side-effect-
/// free, matching how `session.rs`'s idle reaper and
/// `sessions::recover_stale_active_sessions` are already separate steps the
/// daemon's startup sequence calls explicitly, not hidden inside a `new`.
///
/// Used via `WorkflowEngine::init_project_workflows` (issue #88) to seed a
/// project repo's own `.chocofactory/workflows/`. The daemon itself no
/// longer seeds anything at startup (#129): it regenerates its private copy
/// with [`materialize_builtins`].
pub fn seed_builtin_workflows(workflows_dir: &Path) -> io::Result<SeedReport> {
    std::fs::create_dir_all(workflows_dir)?;
    std::fs::create_dir_all(workflows_dir.join("prompts"))?;
    std::fs::create_dir_all(workflows_dir.join("scripts"))?;
    let mut report = SeedReport::default();
    for (relative, source, mode) in builtin_files() {
        let path = workflows_dir.join(relative);
        if seed_one(&path, source, mode)? {
            report.created.push(path);
        } else {
            report.existing.push(path);
        }
    }
    Ok(report)
}

/// Every embedded file as `(relative path, contents, mode for seeding)`:
/// `<name>.yaml` and `prompts/<name>` at 0o644, `scripts/<name>` at 0o755.
pub fn builtin_files() -> Vec<(PathBuf, &'static str, u32)> {
    let mut files = Vec::new();
    for (name, source) in BUILTIN_WORKFLOWS {
        files.push((PathBuf::from(format!("{name}.yaml")), *source, 0o644));
    }
    for (name, source) in BUILTIN_WORKFLOW_PROMPTS {
        files.push((Path::new("prompts").join(name), *source, 0o644));
    }
    for (name, source) in BUILTIN_WORKFLOW_SCRIPTS {
        files.push((Path::new("scripts").join(name), *source, 0o755));
    }
    files
}

const README_NAME: &str = "README.txt";
const README_TEXT: &str = "Generated by chocofactoryd from the workflows built into its binary, at every start.\nEdits here are overwritten. To customise a workflow, copy the built-ins into a repo with\n`choco project init-workflows <project>`, or run your own file with\n`choco task create --workflow <path-to.yaml>`.\n";

fn set_dir_mode(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
}

/// Writes `contents` to `path` with exactly `mode`, unless the file already
/// has those bytes and permission bits (then it is left alone, mtime
/// included). Otherwise: temp file in the same directory, `sync_all`, an
/// explicit `set_permissions` (a leftover temp keeps its old mode), `rename`.
fn write_if_different(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.is_file()
        && meta.permissions().mode() & 0o7777 == mode
        && std::fs::read(path).is_ok_and(|bytes| bytes == contents)
    {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!(".{file_name}.choco-new"));
    // A leftover from a crashed start may be read-only (or a planted
    // symlink): remove it, never open through it.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(with_path(err, "remove", &tmp)),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| with_path(err, "create", &tmp))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|err| with_path(err, "write", &tmp))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|err| with_path(err, "set permissions on", &tmp))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|err| with_path(err, "rename into place", path))
}

/// Adds the failing operation and path to an I/O error, keeping its kind.
fn with_path(err: io::Error, action: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{action} {}: {err}", path.display()))
}

/// Regenerates the daemon's private, read-only copy of the built-in
/// workflows in `dir` (#129). Run at every start, before anything loads a
/// workflow; the daemon lock makes this the only writer. Anything in `dir`,
/// `prompts/` or `scripts/` that isn't a current built-in (or `README.txt`)
/// is deleted. Any I/O error propagates.
pub fn materialize_builtins(dir: &Path) -> io::Result<()> {
    set_dir_mode(dir).map_err(|e| with_path(e, "prepare", dir))?;
    // A `prompts`/`scripts` that is a symlink or a file must not be written
    // through: remove it so it is recreated as a real directory.
    for sub in ["prompts", "scripts"] {
        let path = dir.join(sub);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => std::fs::remove_file(&path).map_err(|e| with_path(e, "remove", &path))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    set_dir_mode(&dir.join("prompts"))
        .map_err(|e| with_path(e, "prepare", &dir.join("prompts")))?;
    set_dir_mode(&dir.join("scripts"))
        .map_err(|e| with_path(e, "prepare", &dir.join("scripts")))?;

    let mut keep: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    keep.insert(PathBuf::from(README_NAME));
    for (relative, source, seed_mode) in builtin_files() {
        let mode = if seed_mode & 0o111 != 0 { 0o555 } else { 0o444 };
        write_if_different(&dir.join(&relative), source.as_bytes(), mode)?;
        keep.insert(relative);
    }
    write_if_different(&dir.join(README_NAME), README_TEXT.as_bytes(), 0o444)?;

    for sub in [None, Some("prompts"), Some("scripts")] {
        let base = match sub {
            Some(s) => dir.join(s),
            None => dir.to_path_buf(),
        };
        for entry in std::fs::read_dir(&base).map_err(|e| with_path(e, "list", &base))? {
            let entry = entry.map_err(|e| with_path(e, "list", &base))?;
            let relative = match sub {
                Some(s) => Path::new(s).join(entry.file_name()),
                None => PathBuf::from(entry.file_name()),
            };
            if keep.contains(&relative) {
                continue;
            }
            // `file_type` does not follow symlinks.
            let file_type = entry
                .file_type()
                .map_err(|e| with_path(e, "inspect", &entry.path()))?;
            if file_type.is_dir() {
                // Our own subdirectories are kept; a directory elsewhere is
                // stray content in a directory the daemon owns.
                if sub.is_none()
                    && (relative == Path::new("prompts") || relative == Path::new("scripts"))
                {
                    continue;
                }
                std::fs::remove_dir_all(entry.path())
                    .map_err(|e| with_path(e, "remove", &entry.path()))?;
            } else {
                std::fs::remove_file(entry.path())
                    .map_err(|e| with_path(e, "remove", &entry.path()))?;
            }
        }
    }
    Ok(())
}

/// What [`scan_legacy_workflows`] found in the old global workflows folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyScan {
    pub dir: PathBuf,
    /// Files identical to a current built-in at the same relative path.
    pub stale: usize,
    /// Everything else (edited copies, custom files, symlinks), sorted.
    pub other: Vec<PathBuf>,
}

/// Reports on the old `~/.config/chocofactory/workflows/` (#129). `None` if
/// `dir` doesn't exist. Walks without following symlinks (a symlink counts
/// as "other"). Never writes, renames or deletes anything.
pub fn scan_legacy_workflows(dir: &Path) -> io::Result<Option<LegacyScan>> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    }
    let builtins: std::collections::HashMap<PathBuf, &'static str> = builtin_files()
        .into_iter()
        .map(|(relative, source, _)| (relative, source))
        .collect();
    let mut scan = LegacyScan {
        dir: dir.to_path_buf(),
        stale: 0,
        other: Vec::new(),
    };
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let relative = path.strip_prefix(dir).unwrap_or(&path);
                match builtins.get(relative) {
                    Some(source) if std::fs::read(&path)? == source.as_bytes() => scan.stale += 1,
                    _ => scan.other.push(path),
                }
            } else {
                scan.other.push(path);
            }
        }
    }
    scan.other.sort();
    Ok(Some(scan))
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

    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    fn mtime_of(path: &Path) -> std::time::SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    /// Names, bytes and modes of every entry under `dir`, symlinks unfollowed.
    fn tree_fingerprint(dir: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
        let mut out = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in std::fs::read_dir(&current).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                let mode = meta.permissions().mode();
                if meta.is_dir() {
                    out.push((path.clone(), Vec::new(), mode));
                    pending.push(path);
                } else if meta.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    out.push((
                        path,
                        target.to_string_lossy().into_owned().into_bytes(),
                        mode,
                    ));
                } else {
                    out.push((path.clone(), std::fs::read(&path).unwrap(), mode));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn builtin_files_lists_exactly_the_three_tables() {
        let files = builtin_files();
        assert_eq!(
            files.len(),
            BUILTIN_WORKFLOWS.len()
                + BUILTIN_WORKFLOW_PROMPTS.len()
                + BUILTIN_WORKFLOW_SCRIPTS.len()
        );
        for (name, source) in BUILTIN_WORKFLOWS {
            assert!(files.contains(&(PathBuf::from(format!("{name}.yaml")), *source, 0o644)));
        }
        for (name, source) in BUILTIN_WORKFLOW_PROMPTS {
            assert!(files.contains(&(Path::new("prompts").join(name), *source, 0o644)));
        }
        for (name, source) in BUILTIN_WORKFLOW_SCRIPTS {
            assert!(files.contains(&(Path::new("scripts").join(name), *source, 0o755)));
        }
    }

    #[test]
    fn materialize_writes_every_builtin_read_only() {
        let dir = TempDir::new();
        materialize_builtins(&dir.path).unwrap();
        for (relative, source, seed_mode) in builtin_files() {
            let path = dir.path.join(&relative);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
            let expected = if seed_mode & 0o111 != 0 { 0o555 } else { 0o444 };
            assert_eq!(mode_of(&path), expected, "{relative:?}");
        }
        let readme = dir.path.join("README.txt");
        assert_eq!(
            std::fs::read_to_string(&readme).unwrap(),
            "Generated by chocofactoryd from the workflows built into its binary, at every start.\n\
Edits here are overwritten. To customise a workflow, copy the built-ins into a repo with\n\
`choco project init-workflows <project>`, or run your own file with\n\
`choco task create --workflow <path-to.yaml>`.\n"
        );
        assert_eq!(mode_of(&readme), 0o444);
        for sub in [
            &dir.path,
            &dir.path.join("prompts"),
            &dir.path.join("scripts"),
        ] {
            assert_eq!(mode_of(sub), 0o755);
        }
    }

    #[test]
    fn a_symlinked_subdirectory_is_replaced_not_written_through() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::create_dir_all(&dir.path).unwrap();
        std::fs::create_dir_all(&outside.path).unwrap();
        std::os::unix::fs::symlink(&outside.path, dir.path.join("prompts")).unwrap();
        materialize_builtins(&dir.path).unwrap();
        let meta = std::fs::symlink_metadata(dir.path.join("prompts")).unwrap();
        assert!(meta.file_type().is_dir());
        assert_eq!(std::fs::read_dir(&outside.path).unwrap().count(), 0);
        assert!(dir.path.join("prompts").join("coder-turn.md").exists());
    }

    #[test]
    fn materialize_again_is_idempotent_and_repairs_drift() {
        let dir = TempDir::new();
        materialize_builtins(&dir.path).unwrap();
        let files: Vec<PathBuf> = builtin_files()
            .into_iter()
            .map(|(relative, _, _)| dir.path.join(relative))
            .collect();
        let before: Vec<_> = files.iter().map(|p| mtime_of(p)).collect();

        // Drift: an edited file, a right-bytes-wrong-mode file, strays, a
        // symlink, and a leftover temp file with a loose mode.
        let chat = dir.path.join("chat.yaml");
        let coding = dir.path.join("coding-task.yaml");
        let prompt = dir.path.join("prompts").join("coder-turn.md");
        for path in [&chat, &coding, &prompt] {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
        }
        std::fs::write(&chat, "edited").unwrap();
        // `coding` keeps its bytes but gets mode 0644 (above).
        std::fs::write(dir.path.join("extra.yaml"), "x").unwrap();
        std::fs::write(dir.path.join("prompts").join("x.md"), "x").unwrap();
        std::fs::create_dir_all(dir.path.join("prompts").join("sub").join("deep")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path.join("scripts").join("link")).unwrap();
        let leftover = dir.path.join(".coding-task.yaml.choco-new");
        std::fs::write(&leftover, "stale").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Make the untouched files' mtimes distinguishable from "rewritten".
        std::thread::sleep(std::time::Duration::from_millis(20));

        materialize_builtins(&dir.path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&chat).unwrap(),
            BUILTIN_WORKFLOWS[0].1
        );
        assert_eq!(mode_of(&chat), 0o444);
        assert_eq!(mode_of(&coding), 0o444);
        assert!(!dir.path.join("extra.yaml").exists());
        assert!(!dir.path.join("prompts").join("x.md").exists());
        assert!(
            !dir.path.join("prompts").join("sub").exists(),
            "stray dir removed"
        );
        assert!(
            std::fs::symlink_metadata(dir.path.join("scripts").join("link")).is_err(),
            "symlink removed"
        );
        assert!(!leftover.exists(), "leftover temp removed");
        // Untouched files keep their mtime (and the temp's 0600 never leaks).
        for (path, mtime) in files.iter().zip(&before) {
            if *path == chat || *path == coding {
                continue;
            }
            if *path == prompt {
                continue;
            }
            assert_eq!(mtime_of(path), *mtime, "{path:?} was rewritten");
        }
        for path in &files {
            assert_ne!(mode_of(path) & 0o200, 0o200, "{path:?} is writable");
        }
        // A third run changes nothing at all.
        let fingerprint = tree_fingerprint(&dir.path);
        let mtimes: Vec<_> = files.iter().map(|p| mtime_of(p)).collect();
        materialize_builtins(&dir.path).unwrap();
        assert_eq!(tree_fingerprint(&dir.path), fingerprint);
        assert_eq!(
            files.iter().map(|p| mtime_of(p)).collect::<Vec<_>>(),
            mtimes
        );
    }

    #[test]
    fn a_leftover_temp_file_does_not_leak_its_mode_into_the_target() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.path).unwrap();
        let leftover = dir.path.join(".chat.yaml.choco-new");
        std::fs::write(&leftover, "stale").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o600)).unwrap();
        materialize_builtins(&dir.path).unwrap();
        assert_eq!(mode_of(&dir.path.join("chat.yaml")), 0o444);
    }

    #[test]
    fn a_read_only_leftover_temp_file_does_not_block_materializing() {
        let dir = TempDir::new();
        materialize_builtins(&dir.path).unwrap();
        let chat = dir.path.join("chat.yaml");
        std::fs::set_permissions(&chat, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&chat, "edited").unwrap();
        let leftover = dir.path.join(".chat.yaml.choco-new");
        std::fs::write(&leftover, "stale").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o444)).unwrap();
        materialize_builtins(&dir.path).unwrap();
        assert!(!leftover.exists());
        assert_eq!(mode_of(&chat), 0o444);
        assert_eq!(
            std::fs::read_to_string(&chat).unwrap(),
            BUILTIN_WORKFLOWS[0].1
        );
    }

    #[test]
    fn materialize_errors_name_the_path() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.path).unwrap();
        // A directory where the temp file must go cannot be removed with remove_file.
        std::fs::create_dir_all(dir.path.join(".chat.yaml.choco-new")).unwrap();
        let err = materialize_builtins(&dir.path).unwrap_err();
        assert!(err.to_string().contains(".chat.yaml.choco-new"), "{err}");
    }

    #[test]
    fn scan_of_a_missing_dir_is_none() {
        let dir = TempDir::new();
        assert_eq!(scan_legacy_workflows(&dir.path).unwrap(), None);
    }

    #[test]
    fn scan_counts_identical_copies_as_stale_and_reports_the_rest_without_writing() {
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let total = builtin_files().len();

        let scan = scan_legacy_workflows(&dir.path).unwrap().unwrap();
        assert_eq!(scan.stale, total);
        assert!(scan.other.is_empty(), "{scan:?}");

        std::fs::write(dir.path.join("coding-task.yaml"), "edited\n").unwrap();
        std::fs::write(dir.path.join("prompts").join("coder-turn.md"), "mine").unwrap();
        std::fs::write(dir.path.join("mine.yaml"), "name: mine\n").unwrap();
        std::os::unix::fs::symlink("chat.yaml", dir.path.join("alias.yaml")).unwrap();

        let before = tree_fingerprint(&dir.path);
        let scan = scan_legacy_workflows(&dir.path).unwrap().unwrap();
        assert_eq!(tree_fingerprint(&dir.path), before, "scan must not write");
        assert_eq!(scan.dir, dir.path);
        assert_eq!(scan.stale, total - 2);
        let mut expected = vec![
            dir.path.join("alias.yaml"),
            dir.path.join("coding-task.yaml"),
            dir.path.join("mine.yaml"),
            dir.path.join("prompts").join("coder-turn.md"),
        ];
        expected.sort();
        assert_eq!(scan.other, expected);
    }

    #[test]
    fn seeds_missing_builtin_workflows_and_creates_the_directory() {
        let dir = TempDir::new();
        assert!(!dir.path.exists());

        let report = seed_builtin_workflows(&dir.path).unwrap();
        assert!(report.existing.is_empty(), "{report:?}");
        assert!(
            report.created.contains(&dir.path.join("chat.yaml")),
            "{report:?}"
        );
        assert!(
            report.created.contains(&dir.path.join("coding-task.yaml")),
            "{report:?}"
        );
        assert_eq!(
            report.created.len(),
            BUILTIN_WORKFLOWS.len()
                + BUILTIN_WORKFLOW_PROMPTS.len()
                + BUILTIN_WORKFLOW_SCRIPTS.len(),
            "{report:?}"
        );

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
        let second = seed_builtin_workflows(&dir.path).unwrap();
        assert!(second.created.is_empty(), "{second:?}");
        assert_eq!(
            second.existing.len(),
            BUILTIN_WORKFLOWS.len()
                + BUILTIN_WORKFLOW_PROMPTS.len()
                + BUILTIN_WORKFLOW_SCRIPTS.len(),
            "{second:?}"
        );
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

    /// Both built-ins carry the same review-gate backoff schedule.
    #[test]
    fn both_built_ins_back_the_review_gate_off_over_102_hours() {
        use crate::workflow_def::BackoffStep;
        use std::time::Duration;
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        for file in ["coding-task.yaml", "coding-task-planned.yaml"] {
            let def = crate::workflow_def::WorkflowDefinition::load(&dir.path.join(file)).unwrap();
            let watch = def.stages["awaiting_human_review"]
                .watch()
                .expect("awaiting_human_review has a watcher");
            assert_eq!(watch.interval, Duration::from_secs(60), "{file}");
            assert_eq!(
                watch.backoff,
                [
                    BackoffStep {
                        after: Duration::from_secs(6 * 3600),
                        interval: Duration::from_secs(300)
                    },
                    BackoffStep {
                        after: Duration::from_secs(30 * 3600),
                        interval: Duration::from_secs(1800)
                    },
                ],
                "{file}"
            );
            assert_eq!(
                watch.timeout,
                Some(Duration::from_secs(102 * 3600)),
                "{file}"
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
        for (stage, on, max) in [
            ("internal_review", "changes_requested", 3),
            ("checks_polling", "red", 3),
        ] {
            let guard = def.stages[stage].loop_guard.as_ref().expect("loop guard");
            assert_eq!(
                (guard.on.as_str(), guard.max, guard.then.as_str()),
                (on, max, "escalate_to_human"),
                "{stage}"
            );
        }

        use crate::workflow_def::{Capture, ShellCommand, StageKind};
        let stage = &def.stages["awaiting_human_review"];
        let StageKind::HumanGate {
            capture,
            markers,
            watch,
        } = &stage.kind
        else {
            panic!(
                "awaiting_human_review must be a human_gate: {:?}",
                stage.kind
            );
        };
        assert_eq!(*capture, Some(Capture::Text));
        let markers: Vec<(&str, &str)> = markers
            .iter()
            .map(|m| (m.line.as_str(), m.then.as_str()))
            .collect();
        assert_eq!(
            markers,
            [
                ("/request-changes", "changes_requested"),
                ("/approve", "approved")
            ]
        );
        let watch = watch.as_ref().expect("awaiting_human_review has a watcher");
        match &watch.command {
            ShellCommand::ScriptFile(path) => assert!(path.ends_with("await-review.sh")),
            other => panic!("expected a script file, got {other:?}"),
        }
        let env: Vec<(&str, &str)> = watch
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            env,
            [
                ("PR_NUMBER", "{{ stages.open_pr.number }}"),
                ("HANDED_OVER_AT", "{{ left_at.awaiting_human_review }}"),
            ]
        );
        assert_eq!(watch.interval, std::time::Duration::from_secs(60));
        let outcomes: Vec<(&str, &str)> = watch
            .outcomes
            .iter()
            .map(|o| (o.pattern.as_str(), o.then.as_str()))
            .collect();
        assert_eq!(
            outcomes,
            [
                (r"\AREQUEST_CHANGES(\n|$)", "changes_requested"),
                (r"\AAPPROVE(\n|$)", "approved"),
                (r"\AMERGED(\n|$)", "approved"),
            ]
        );
        let on: Vec<(&str, &str)> = stage
            .on
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            on,
            [
                ("approved", "done"),
                ("changes_requested", "revising"),
                ("timeout", "escalate_to_human")
            ]
        );
        let guard = stage.loop_guard.as_ref().expect("loop guard");
        assert_eq!(
            (guard.on.as_str(), guard.max, guard.then.as_str()),
            ("changes_requested", 3, "escalate_to_human")
        );
    }

    /// The workflow's `markers:` and the shared case table agree: a marker
    /// renamed in the YAML alone fails here.
    #[test]
    fn the_seeded_review_markers_match_the_shared_case_table() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            body: String,
            choco: String,
        }
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let def = crate::workflow_def::WorkflowDefinition::load(&dir.path.join("coding-task.yaml"))
            .unwrap();
        let crate::workflow_def::StageKind::HumanGate { markers, .. } =
            &def.stages["awaiting_human_review"].kind
        else {
            panic!("awaiting_human_review must be a human_gate");
        };
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../tests/fixtures/review-markers.json")).unwrap();
        assert!(cases.len() >= 16);
        for case in cases {
            let label = match crate::engine::reply_verdict(&case.body, markers) {
                Ok(v) => v.outcome.to_string(),
                Err(err) => match format!("{err:?}") {
                    d if d.starts_with("NoMarker") => "refused_no_marker".to_string(),
                    d if d.starts_with("Conflict") => "refused_conflict".to_string(),
                    d => panic!("case '{}': unexpected refusal {d}", case.name),
                },
            };
            assert_eq!(label, case.choco, "case '{}'", case.name);
        }
    }

    fn coder_revise_content() -> &'static str {
        BUILTIN_WORKFLOW_PROMPTS
            .iter()
            .find(|(name, _)| *name == "coder-revise.md")
            .expect("coder-revise.md must be a seeded prompt")
            .1
    }

    /// #112: the embedded `coder-revise.md` branches on `{{ arrival.from }}`/
    /// `{{ arrival.outcome }}`, not on which of `stages.internal_review.
    /// summary`/`stages.escalate_to_human` happens to be non-empty — both
    /// captures persist across laps once either stage has ever run, so a
    /// stale one must never be mistaken for the actual reason the coder is
    /// back. Each of these four tests renders the real embedded prompt for
    /// one arrival path, with a stale-looking capture seeded in the
    /// "wrong" slot, and checks that: the "You are back here because"
    /// sentence names the real arrival, and any seeded capture text only
    /// ever appears after its own heading.
    fn assert_coder_revise_names_arrival_and_isolates_captures(
        from: &str,
        outcome: &str,
        stages: serde_json::Value,
    ) -> String {
        let payload = serde_json::json!({
            "task": { "title": "Fix the thing", "input": "do the thing" },
            "arrival": { "from": from, "outcome": outcome },
            "stages": stages,
        });
        let (rendered, _unresolved) = crate::template::render(coder_revise_content(), &payload)
            .unwrap_or_else(|err| panic!("coder-revise.md failed to render: {err}"));

        assert!(
            rendered.contains(&format!(
                "the `{from}` stage ended with the outcome `{outcome}`"
            )),
            "expected the arrival sentence to name {from}/{outcome}:\n{rendered}"
        );

        let path_list_start = rendered
            .find("- **`internal_review`")
            .expect("path list must be present");
        let reviewer_heading = rendered
            .find("## Internal reviewer's summary")
            .expect("reviewer summary heading must be present");
        let human_note_heading = rendered
            .find("## A human's note")
            .expect("human note heading must be present");
        assert!(path_list_start < reviewer_heading);
        assert!(reviewer_heading < human_note_heading);

        rendered
    }

    /// One route's bullet out of the rendered path list: from its
    /// "- **`<from>`" marker to the next bullet or the blank line that ends
    /// the list. Lets an assertion pin text to the entry that must carry it,
    /// rather than to anywhere in the prompt.
    fn route_entry<'a>(rendered: &'a str, from: &str) -> &'a str {
        let marker = format!("- **`{from}`");
        let start = rendered
            .find(&marker)
            .unwrap_or_else(|| panic!("no route entry for {from}:\n{rendered}"));
        let rest = &rendered[start + marker.len()..];
        let end = [rest.find("\n- **`"), rest.find("\n\n")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        &rendered[start..start + marker.len() + end]
    }

    /// `text` with every run of whitespace collapsed to one space, so a
    /// phrase assertion doesn't depend on where the prompt wraps its lines.
    fn squash(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Asserts that `text` (already squashed) says each of `clauses` word
    /// for word. Whole clauses, not keywords: a rewrite that keeps the
    /// keywords but changes what the sentence tells the coder to do must
    /// fail.
    fn assert_says(text: &str, clauses: &[&str], what: &str) {
        for clause in clauses {
            assert!(
                text.contains(clause),
                "{what}: expected \"{clause}\" in:\n{text}"
            );
        }
    }

    #[test]
    fn coder_revise_for_the_internal_review_path_names_the_arrival() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "internal_review",
            "changes_requested",
            serde_json::json!({ "internal_review": { "summary": "CURRENT REVIEWER SUMMARY" } }),
        );
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        let human_note_heading = rendered.find("## A human's note").unwrap();
        let summary = rendered.find("CURRENT REVIEWER SUMMARY").unwrap();
        assert_eq!(rendered.matches("CURRENT REVIEWER SUMMARY").count(), 1);
        assert!(
            reviewer_heading < summary && summary < human_note_heading,
            "the reviewer's summary must render only under its own heading:\n{rendered}"
        );
    }

    /// A stale internal approval and a human's review, as the payload holds
    /// them on a `/request-changes` lap (#138).
    fn stale_approval_and_human_review() -> serde_json::Value {
        serde_json::json!({
            "internal_review": { "summary": "STALE REVIEWER APPROVAL" },
            "awaiting_human_review":
                "REQUEST_CHANGES\n\n### owner (OWNER), 2026-10-03T00:31:00Z\n\
                 https://example.test/c/1\n\nHUMAN ITEM: fix the release build"
        })
    }

    #[test]
    fn coder_revise_for_the_awaiting_human_review_path_hands_over_the_humans_review() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "awaiting_human_review",
            "changes_requested",
            stale_approval_and_human_review(),
        );
        let human_heading = rendered.find("## The human's review").unwrap();
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        let human_note_heading = rendered.find("## A human's note").unwrap();
        assert!(human_heading < reviewer_heading);

        // The human's comment renders once, under its own heading.
        assert_eq!(rendered.matches("HUMAN ITEM").count(), 1);
        let item = rendered.find("HUMAN ITEM").unwrap();
        assert!(
            human_heading < item && item < reviewer_heading,
            "the human's review must render only under its own heading:\n{rendered}"
        );
        assert_says(
            &squash(&rendered[human_heading..reviewer_heading]),
            &["Current on the `awaiting_human_review` path."],
            "the human's review label must say it is current on this path",
        );

        // The stale summary renders once, under its own heading, labelled
        // stale on this path (#112's whole point).
        let summary = rendered.find("STALE REVIEWER APPROVAL").unwrap();
        assert_eq!(rendered.matches("STALE REVIEWER APPROVAL").count(), 1);
        assert!(
            reviewer_heading < summary && summary < human_note_heading,
            "the stale summary must render only under its own heading:\n{rendered}"
        );
        assert_says(
            &squash(&rendered[reviewer_heading..summary]),
            &[
                "On the `awaiting_human_review` path it is stale",
                "the human's review above is the one to act on.",
            ],
            "the reviewer summary's label must say it is stale on this path",
        );

        // The route entry points at the section and keeps the fallback.
        let entry = squash(route_entry(&rendered, "awaiting_human_review"));
        assert_says(
            &entry,
            &[
                "Their comments are quoted below under \"The human's review\", and that \
                 section is current.",
                "Address every item in it, not just the first.",
                "Formal review bodies and their inline review comments are in that section too",
                r#"N=$(gh pr list --head "$(git rev-parse --abbrev-ref HEAD)" --state open"#,
                r#"gh api --paginate "repos/{owner}/{repo}/pulls/$N/reviews""#,
                r#"gh api --paginate "repos/{owner}/{repo}/pulls/$N/comments""#,
                "`author_association` OWNER, MEMBER or COLLABORATOR, and a `user.login` that \
                 doesn't end in `[bot]`.",
                "The internal reviewer's summary below is **not** this feedback.",
            ],
            "the awaiting_human_review entry",
        );
        assert!(!entry.contains("Run `gh pr view --comments`"));
    }

    /// A review sent through choco has no verdict line; the prompt tells the
    /// coder to read the PR's top-level comments as well.
    #[test]
    fn coder_revise_for_a_choco_review_says_to_read_the_top_level_comments() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "awaiting_human_review",
            "changes_requested",
            serde_json::json!({ "awaiting_human_review": "CHOCO REVIEW: fix the build" }),
        );
        let human_heading = rendered.find("## The human's review").unwrap();
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        assert_eq!(rendered.matches("CHOCO REVIEW: fix the build").count(), 1);
        let at = rendered.find("CHOCO REVIEW: fix the build").unwrap();
        assert!(human_heading < at && at < reviewer_heading, "{rendered}");

        let entry = squash(route_entry(&rendered, "awaiting_human_review"));
        assert_says(
            &entry,
            &[
                r#"gh api --paginate "repos/{owner}/{repo}/issues/$N/comments""#,
                "If \"The human's review\" doesn't start with the line `REQUEST_CHANGES`, the \
                 human answered through choco",
            ],
            "the awaiting_human_review entry covers a review sent through choco",
        );
    }

    /// On the `internal_review` path the human's review is from an earlier
    /// lap, already handled, and must be labelled so.
    #[test]
    fn coder_revise_for_the_internal_review_path_labels_the_humans_review_as_handled() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "internal_review",
            "changes_requested",
            stale_approval_and_human_review(),
        );
        let entry = squash(route_entry(&rendered, "internal_review"));
        assert_says(
            &entry,
            &[
                "and it is current. Address every finding in it.",
                "If \"The human's review\" below has content, it is from an earlier lap and already handled on this branch: each item was either done or declined with a reason. Don't redo it, don't take up a declined item, and don't undo it. If a finding would undo a change the human asked for, keep the human's change and say so in your summary.",
            ],
            "the internal_review entry names its own section as current and the human's review as handled",
        );
        let human_heading = rendered.find("## The human's review").unwrap();
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        assert_says(
            &squash(&rendered[human_heading..reviewer_heading]),
            &[HUMAN_LABEL_OTHER_PATHS],
            "the human's review label must say it is already handled here",
        );
    }

    /// The label's wording for every path but the two that use the review.
    const HUMAN_LABEL_OTHER_PATHS: &str = "On any other path it is from an earlier lap and already handled on this branch (each item done or declined with a reason): don't redo it, and don't undo it.";

    /// On the `checks_polling` → `red` path the human's review is likewise
    /// handled: CI must be fixed without undoing it.
    #[test]
    fn coder_revise_for_the_checks_polling_red_path_labels_the_humans_review_as_handled() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "checks_polling",
            "red",
            stale_approval_and_human_review(),
        );
        let entry = squash(route_entry(&rendered, "checks_polling"));
        assert_says(
            &entry,
            &[
                "If \"The human's review\" below has content, it is from an earlier lap and already handled on this branch (each item done or declined with a reason): fix the failure without undoing the human's change. If the only fix would undo it, say so in your summary.",
            ],
            "the checks_polling entry says the human's review is handled",
        );
        let human_heading = rendered.find("## The human's review").unwrap();
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        assert_says(
            &squash(&rendered[human_heading..reviewer_heading]),
            &[HUMAN_LABEL_OTHER_PATHS],
            "the human's review label must say it is already handled here",
        );
    }

    /// Comment text is data: a `{{ ... }}` in it must not be substituted.
    #[test]
    fn coder_revise_renders_a_human_comment_literally() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "awaiting_human_review",
            "changes_requested",
            serde_json::json!({
                "awaiting_human_review": "REQUEST_CHANGES\n\nplease keep {{ task.input }} as is"
            }),
        );
        assert!(
            rendered.contains("please keep {{ task.input }} as is"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("do the thing").count(), 1, "{rendered}");
    }

    #[test]
    fn coder_revise_allows_an_empty_commit_only_for_description_only_changes() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "awaiting_human_review",
            "changes_requested",
            serde_json::json!({}),
        );
        let done = rendered.find("## When you're done").unwrap();
        let done = squash(&rendered[done..]);
        assert_says(
            &done,
            &[
                "is allowed only when every requested change is to the PR's description",
                "On the `awaiting_human_review` path, map each item in the human's review to \
                 the short SHA of the commit that addresses it, or say it wasn't done and why.",
            ],
            "the closing section's empty-commit rule",
        );
    }

    /// R3: every item sent back gets a line in the `report_outcome`
    /// summary, including ones the coder can't act on (#110's dropped
    /// PR-description item). The section is the same on every path.
    #[test]
    fn coder_revise_asks_for_one_line_per_item_sent_back() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "awaiting_human_review",
            "changes_requested",
            serde_json::json!({}),
        );
        let done = rendered
            .find("## When you're done")
            .expect("the closing section must be present");
        assert_says(
            &squash(&rendered[done..]),
            &[
                "Commit your revisions, and rewrite the PR description file whole (step 4 of \
                 your instructions) so it describes the branch as it now stands.",
                "An empty commit (`git commit --allow-empty -m \"Update the PR description: \
                 <why>\"`) is allowed only when every requested change is to the PR's \
                 description",
                "In its summary, give one short line per item you were sent back for: what \
                 you changed, or that you didn't act on it and why.",
                "A requested change to the PR's description is done by rewriting that file; the \
                 workflow republishes it.",
                "The PR's title comes from the task and can't be changed from here. List a \
                 title change, and anything else you can't do from here, as not done rather \
                 than leaving it out.",
            ],
            "the closing section must ask for an account of every item",
        );
    }

    #[test]
    fn coder_revise_for_the_checks_polling_path_names_the_arrival() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "checks_polling",
            "red",
            serde_json::json!({ "internal_review": { "summary": "STALE REVIEWER APPROVAL" } }),
        );
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        let human_note_heading = rendered.find("## A human's note").unwrap();
        let summary = rendered.find("STALE REVIEWER APPROVAL").unwrap();
        assert!(
            reviewer_heading < summary && summary < human_note_heading,
            "the stale summary must render only under its own heading:\n{rendered}"
        );
        assert_eq!(rendered.matches("STALE REVIEWER APPROVAL").count(), 1);
    }

    #[test]
    fn coder_revise_for_the_escalate_to_human_path_isolates_the_human_note() {
        let rendered = assert_coder_revise_names_arrival_and_isolates_captures(
            "escalate_to_human",
            "resumed",
            serde_json::json!({
                "internal_review": { "summary": "STALE REVIEWER APPROVAL" },
                "escalate_to_human": "CURRENT HUMAN NOTE",
            }),
        );
        let reviewer_heading = rendered.find("## Internal reviewer's summary").unwrap();
        let human_note_heading = rendered.find("## A human's note").unwrap();
        let note = rendered.find("CURRENT HUMAN NOTE").unwrap();
        assert_eq!(rendered.matches("CURRENT HUMAN NOTE").count(), 1);
        assert!(
            human_note_heading < note,
            "the human's note must render only under its own heading:\n{rendered}"
        );

        // When the PR review's loop guard tripped, the reviewer's summary is
        // the approval that opened the PR: it must sit under its own
        // heading, framed as context the note outranks.
        let summary = rendered.find("STALE REVIEWER APPROVAL").unwrap();
        assert_eq!(rendered.matches("STALE REVIEWER APPROVAL").count(), 1);
        assert!(
            reviewer_heading < summary && summary < human_note_heading,
            "the stale summary must render only under its own heading:\n{rendered}"
        );
        // The entry must not present the captured human review as current
        // on every escalation: only when the review's loop guard tripped.
        assert!(
            squash(&rendered).contains(
                "When the escalation came from the PR review's loop guard, the section \"The \
                 human's review\" below is that review; on any other escalation it may be left over"
            ),
            "the escalate entry must make the human-review reference conditional:\n{rendered}"
        );
        let framing = squash(&rendered[reviewer_heading..summary]);
        // Either loop guard can escalate; the summary is the rejection only
        // for the internal reviewer's. The old wording assumed it always was,
        // and swapping the two conditions would misroute both cases.
        assert_says(
            &framing,
            &[
                "On the `escalate_to_human` path it is context at most, and the human's note \
                 takes priority.",
                "If it rejects your work, the escalation came from the internal reviewer's \
                 loop guard: this is the rejection that tripped it",
                "If it approves, it's the internal reviewer's last approval, and any \
                 rejection is in the PR's comments, if a PR is open",
            ],
            "the escalate framing must outrank the summary and tell the two loop guards apart",
        );
        assert!(
            !framing.contains("may be the rejection that tripped the loop guard"),
            "the old single-guard wording must be gone:\n{framing}"
        );

        // A note like "same issues, keep going" points at the PR, so the
        // entry must send the coder there too.
        let entry = squash(route_entry(&rendered, "escalate_to_human"));
        // ...with the PR route's author fence, not around it.
        assert_says(
            &entry,
            &[
                "If this branch has an open PR, also read everything on it posted or edited \
                 after your last commit, using the fallback commands and the rule about whose \
                 comments are instructions from the `awaiting_human_review` entry, plus top-level \
                 comments: `gh api --paginate \"repos/{owner}/{repo}/issues/$N/comments\"`.",
                "Where the note and a comment disagree, follow the note.",
            ],
            "the escalate_to_human entry must send the coder to the PR under the same fence",
        );
    }

    /// #95: the sections `internal_review` enforces and the ones its
    /// reviewer prompt asks for are two copies of the same list, and the
    /// failure when they drift is silent and expensive — a reviewer writes
    /// the report it was told to write and the tool rejects it, costing a
    /// retry on every single review.
    #[test]
    fn the_reviewer_prompt_asks_for_every_section_the_stage_enforces() {
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();

        let def = crate::workflow_def::WorkflowDefinition::load(&dir.path.join("coding-task.yaml"))
            .unwrap();
        let crate::workflow_def::StageKind::AgentTurn {
            report_sections, ..
        } = &def.stages["internal_review"].kind
        else {
            panic!("internal_review should be an agent_turn");
        };
        assert!(
            !report_sections.is_empty(),
            "internal_review must require report sections"
        );

        // Anchored to the sentence the reviewer actually follows, not to
        // the file as a whole (review of #95, round 2). "Resources and
        // work" and "Old behaviour" appear elsewhere in the prompt as
        // step-3 walks, so a file-wide `contains` passed while that
        // sentence was two sections short — costing two rejections on
        // every single review, which is the failure this test exists to
        // prevent. Order is checked too: the tool asks for these in order,
        // and a list that drifts out of order teaches the reviewer to
        // write them in an order its own summary section contradicts.
        let system_prompt =
            std::fs::read_to_string(dir.path.join("prompts/reviewer-system.md")).unwrap();
        let marker = "must contain these sections, in order:";
        let (_, after_marker) = system_prompt
            .split_once(marker)
            .unwrap_or_else(|| panic!("reviewer-system.md no longer says {marker:?}"));
        // Collapsed *before* the sentence is cut out, not after (review of
        // #95, round 3): the prompt is hard-wrapped, so a two-word section
        // name can span a line break ("Old\nbehaviour"), and a sentence
        // ending a paragraph has its full stop followed by a newline
        // rather than a space — which made the split below either panic
        // with a misleading message or swallow the next sentence whole.
        let after_marker = after_marker
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let (canonical_list, _) = after_marker
            .split_once(". ")
            .expect("the canonical section list should end in a full stop");

        let mut searched_from = 0;
        for section in report_sections {
            let found = canonical_list[searched_from..]
                .find(section.as_str())
                .unwrap_or_else(|| {
                    panic!(
                        "reviewer-system.md's canonical section list is missing '{section}', or \
                         lists it out of `report_sections:` order: {canonical_list:?}"
                    )
                });
            searched_from += found + section.len();
        }
    }

    fn embedded_prompt(name: &str) -> &'static str {
        BUILTIN_WORKFLOW_PROMPTS
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} must be a seeded prompt"))
            .1
    }

    const DESCRIPTION_PATH_COMMAND: &str =
        r#"echo "$(cd "$(git rev-parse --git-dir)" && pwd)/choco-pr-description.md""#;

    #[test]
    fn coder_system_step_three_forbids_closing_keywords_in_commit_messages() {
        let text = squash(embedded_prompt("coder-system.md"));
        assert_says(
            &text,
            &[
                "Never put a closing keyword followed by an issue reference in a commit message",
                "`Closes #12`, `fixes: #12`, `Resolves owner/repo#12`",
                "GitHub closes that issue when the commit reaches the main branch",
                "write `#12` on its own if you need to mention one.",
            ],
            "coder-system.md step 3 must forbid closing keywords in commit messages",
        );
    }

    #[test]
    fn coder_system_step_four_asks_for_the_pr_description() {
        let text = squash(embedded_prompt("coder-system.md"));
        assert_says(
            &text,
            &[
                "What the pull request says is up to you, through the description file in step 4.",
                "4. Write the pull request's description to the file this command prints:",
                DESCRIPTION_PATH_COMMAND,
                "so it is never committed: don't `git add` it or copy it into the tree.",
                "so write none of those, and no `Closes #…` line.",
                "Write it as a short guide to the change for a human reviewer who hasn't read \
                 the task: concise, in plain English, with no codebase jargon or shorthand of \
                 your own, readable in about two minutes.",
                "Write it on every turn, for the branch as a whole rather than for this turn's \
                 commits, rewriting whatever an earlier turn left there.",
                "Start where the request or the change enters the system",
                "Put small incidental fixes last, together in one item.",
                "5. Call `report_outcome`",
            ],
            "coder-system.md step 4 must ask for the PR description",
        );
        let mut at = 0;
        for heading in [
            "`## Problem`",
            "`## Solution`",
            "`## Changes, in reading order`",
            "`## Look closely at`",
            "`## Review history`",
            "`## Not done`",
        ] {
            let found = text[at..]
                .find(heading)
                .unwrap_or_else(|| panic!("{heading} missing or out of order"));
            at += found + heading.len();
        }
    }

    #[test]
    fn coder_system_step_four_asks_for_a_sketch_of_the_new_order() {
        let text = squash(embedded_prompt("coder-system.md"));
        let step = &text[index_of(&text, "4. Write the pull request's description")
            ..index_of(&text, "5. Call `report_outcome`")];
        let rule = "When the change alters what happens in what order (which function calls \
                    which, a state's transitions, or where files live), end this section with \
                    one small sketch of the new order in a fenced plain-text block, not a \
                    diagram language: a call tree, or a call tree with `+` lines for what was \
                    added and `-` lines for what was removed, keeping only the calls that \
                    matter, in about 15 lines at most. Leave it out for a change of a few \
                    lines or one that changes only text.";
        assert_says(
            step,
            &[rule],
            "coder-system.md step 4 must ask for a sketch",
        );
        let rule_at = index_of(step, "When the change alters what happens in what order");
        assert!(
            index_of(step, "- `## Solution`:") < rule_at
                && rule_at < index_of(step, "- `## Changes, in reading order`:"),
            "the sketch rule must sit inside the Solution bullet"
        );
        let example = &step[index_of(step, "For example:")..];
        let sol = index_of(example, "## Solution");
        let chg = index_of(example, "## Changes, in reading order");
        let plus = index_of(example, "+ resume_interrupted_polls");
        let minus = index_of(example, "- wait on a timer that pauses during sleep");
        assert!(
            sol < plus && plus < chg && sol < minus && minus < chg,
            "the example sketch must sit inside its Solution"
        );
        assert!(
            example[sol..plus].contains("```") && example[minus..chg].contains("```"),
            "the example sketch must be fenced"
        );
    }

    #[test]
    fn reviewer_turn_reads_the_pr_description() {
        let text = squash(embedded_prompt("reviewer-turn.md"));
        assert_says(
            &text,
            &[
                r#"`cat "$(cd "$(git rev-parse --git-dir)" && pwd)/choco-pr-description.md"`"#,
                "A claim in it that the code doesn't bear out is a blocking finding",
                "These are non-blocking findings: a missing description; one that leaves out a \
                 change or a trade-off the diff makes; a change list that doesn't follow the \
                 path a request takes through the code; and one that isn't short and in plain \
                 English.",
            ],
            "reviewer-turn.md must have the reviewer check the PR description",
        );
    }

    #[test]
    fn seeding_writes_the_open_pr_script_executable_and_never_overwrites_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let script = dir.path.join("scripts/open-pr.sh");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "mode {mode:o}");

        std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o600)).unwrap();
        let second = seed_builtin_workflows(&dir.path).unwrap();
        assert!(second.existing.contains(&script), "{second:?}");
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            "#!/bin/sh\nexit 7\n"
        );
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o600,
            "an existing script is never re-chmodded"
        );
    }

    /// The script is embedded verbatim and resolves from the seeded dir.
    #[test]
    fn the_seeded_open_pr_stage_resolves_its_script_file() {
        use crate::workflow_def::{ShellCommand, StageKind};
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let def = crate::workflow_def::WorkflowDefinition::load(&dir.path.join("coding-task.yaml"))
            .unwrap();
        let StageKind::Shell { command, .. } = &def.stages["open_pr"].kind else {
            panic!("open_pr must be a shell stage");
        };
        assert_eq!(
            command,
            &ShellCommand::ScriptFile(dir.path.join("scripts/open-pr.sh"))
        );
        assert_eq!(
            std::fs::read_to_string(dir.path.join("scripts/open-pr.sh")).unwrap(),
            BUILTIN_WORKFLOW_SCRIPTS[0].1
        );
    }

    fn load_seeded(dir: &TempDir, file: &str) -> crate::workflow_def::WorkflowDefinition {
        crate::workflow_def::WorkflowDefinition::load(&dir.path.join(file)).unwrap()
    }

    fn on_map(stage: &crate::workflow_def::StageDef) -> Vec<(&str, &str)> {
        stage
            .on
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// The seeded `checks_polling` stage, pinned field by field.
    #[test]
    fn the_seeded_checks_polling_stage_is_pinned() {
        use crate::workflow_def::{ShellCommand, StageKind};
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        for file in ["coding-task.yaml", "coding-task-planned.yaml"] {
            let def = load_seeded(&dir, file);
            let stage = &def.stages["checks_polling"];
            let StageKind::Poll { watch, .. } = &stage.kind else {
                panic!("checks_polling must be a poll");
            };
            match &watch.command {
                ShellCommand::ScriptFile(path) => {
                    assert!(path.ends_with("scripts/ci-checks.sh"), "{path:?}")
                }
                other => panic!("expected a script file, got {other:?}"),
            }
            let env: Vec<(&str, &str)> = watch
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(env, [("PR_NUMBER", "{{ stages.open_pr.number }}")]);
            assert_eq!(watch.interval, std::time::Duration::from_secs(30));
            assert_eq!(watch.timeout, Some(std::time::Duration::from_secs(30 * 60)));
            let outcomes: Vec<(&str, &str)> = watch
                .outcomes
                .iter()
                .map(|o| (o.pattern.as_str(), o.then.as_str()))
                .collect();
            assert_eq!(
                outcomes,
                [
                    (r"\ARED(\n|$)", "red"),
                    (r"\ASTARTUP_FAILURE(\n|$)", "ci_startup_failure"),
                    (r"\AACTION_REQUIRED(\n|$)", "ci_action_required"),
                    (r"\ACANCELLED(\n|$)", "ci_cancelled"),
                    (r"\AGREEN(\n|$)", "green"),
                    (r"\ANO_CHECKS(\n|$)", "no_checks"),
                ]
            );
            assert_eq!(
                on_map(stage),
                [
                    ("green", "awaiting_human_review"),
                    ("no_checks", "awaiting_human_review"),
                    ("red", "revising"),
                    ("ci_startup_failure", "escalate_to_human"),
                    ("ci_action_required", "escalate_to_human"),
                    ("ci_cancelled", "escalate_to_human"),
                    ("timeout", "escalate_to_human"),
                ]
            );
            let guard = stage.loop_guard.as_ref().expect("loop guard");
            assert_eq!(
                (guard.on.as_str(), guard.max, guard.then.as_str()),
                ("red", 3, "escalate_to_human")
            );
        }
    }

    /// The shared CI case table's first lines, through the seeded stage's
    /// real outcome matching and `on:` map.
    #[test]
    fn the_seeded_ci_outcomes_match_the_shared_case_table() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            first_line: String,
            outcome: Option<String>,
        }
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let def = load_seeded(&dir, "coding-task.yaml");
        let stage = &def.stages["checks_polling"];
        let crate::workflow_def::StageKind::Poll { watch, .. } = &stage.kind else {
            panic!("checks_polling must be a poll");
        };
        let compiled = crate::poll::compile(&watch.outcomes).unwrap();
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../tests/fixtures/ci-checks-cases.json")).unwrap();
        assert!(cases.len() >= 20);
        for case in cases {
            // The script prints the token, a blank line, then check lines.
            let stdout = format!("{}\n\nSUCCESS job-0\n", case.first_line);
            let matched = compiled.matching(&stdout).map(|m| m.then.to_string());
            assert_eq!(matched, case.outcome, "case '{}'", case.name);
            if let Some(outcome) = matched {
                let expected = match outcome.as_str() {
                    "green" | "no_checks" => "awaiting_human_review",
                    "red" => "revising",
                    _ => "escalate_to_human",
                };
                assert_eq!(
                    stage.on.get(&outcome).map(|t| t.to_string()).as_deref(),
                    Some(expected),
                    "case '{}'",
                    case.name
                );
            }
        }
        // A token must be the whole first line: a check name cannot select one.
        assert!(compiled.matching("PENDING\n\nFAILURE_x RED\n").is_none());
        assert!(compiled.matching("REDDISH\n").is_none());
    }

    #[test]
    fn the_seeded_ci_checks_script_equals_the_embedded_one() {
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let embedded = BUILTIN_WORKFLOW_SCRIPTS
            .iter()
            .find(|(name, _)| *name == "ci-checks.sh")
            .expect("ci-checks.sh is embedded")
            .1;
        assert_eq!(
            std::fs::read_to_string(dir.path.join("scripts/ci-checks.sh")).unwrap(),
            embedded
        );
        assert_eq!(
            mode_of(&dir.path.join("scripts/ci-checks.sh")) & 0o111,
            0o111
        );
    }

    /// `coding-task-planned` (#120): the seeded workflow loads and has the
    /// two new stages wired as designed.
    #[test]
    fn the_seeded_coding_task_planned_workflow_loads_and_validates() {
        use crate::workflow_def::{Capture, StageKind};
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let def = load_seeded(&dir, "coding-task-planned.yaml");
        assert_eq!(def.name, "coding-task-planned");
        assert!(def.worktree);
        for (stage, on, max) in [
            ("internal_review", "changes_requested", 3),
            ("checks_polling", "red", 3),
        ] {
            let guard = def.stages[stage].loop_guard.as_ref().expect("loop guard");
            assert_eq!(
                (guard.on.as_str(), guard.max, guard.then.as_str()),
                (on, max, "escalate_to_human"),
                "{stage}"
            );
        }
        assert_eq!(def.start_stage(), "spec_check");
        assert_eq!(
            def.roles["planner"].model.as_deref(),
            Some("claude-opus-5-5")
        );

        let spec_check = &def.stages["spec_check"];
        let StageKind::AgentTurn {
            role,
            capture,
            report_sections,
            ..
        } = &spec_check.kind
        else {
            panic!("spec_check must be an agent_turn");
        };
        assert_eq!(role, "planner");
        assert_eq!(*capture, Some(Capture::Json));
        assert_eq!(
            report_sections,
            &["Checks", "Decisions", "Questions", "Spec"]
        );
        assert_eq!(
            on_map(spec_check),
            [("ready", "coding"), ("needs_input", "spec_questions")]
        );
        assert!(spec_check.loop_guard.is_none());

        let gate = &def.stages["spec_questions"];
        let StageKind::HumanGate { capture, .. } = &gate.kind else {
            panic!("spec_questions must be a human_gate");
        };
        assert_eq!(*capture, Some(Capture::Text));
        assert_eq!(on_map(gate), [("resumed", "spec_check")]);
        assert!(gate.loop_guard.is_none());
    }

    /// Drift guard: every stage from `coding` on, and the `coder`/`reviewer`
    /// roles, are `coding-task`'s, apart from three prompt files.
    #[test]
    fn coding_task_planned_stages_and_roles_match_coding_task() {
        use crate::workflow_def::StageKind;
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let base = load_seeded(&dir, "coding-task.yaml");
        let planned = load_seeded(&dir, "coding-task-planned.yaml");

        let expected: Vec<&str> = ["spec_check", "spec_questions"]
            .into_iter()
            .chain(base.stages.keys().map(String::as_str))
            .collect();
        let actual: Vec<&str> = planned.stages.keys().map(String::as_str).collect();
        assert_eq!(actual, expected);

        let swapped = [
            ("coding", "coder-turn-planned.md"),
            ("revising", "coder-revise-planned.md"),
            ("internal_review", "reviewer-turn-planned.md"),
        ];
        for (name, base_stage) in &base.stages {
            let mut planned_stage = planned.stages[name].clone();
            if let Some((_, file)) = swapped.iter().find(|(n, _)| n == name) {
                let StageKind::AgentTurn { prompt_file, .. } = &mut planned_stage.kind else {
                    panic!("{name} must be an agent_turn");
                };
                assert_eq!(
                    prompt_file.as_ref().and_then(|p| p.file_name()),
                    Some(std::ffi::OsStr::new(file)),
                    "{name} must use {file}"
                );
                let StageKind::AgentTurn {
                    prompt_file: base_file,
                    ..
                } = &base_stage.kind
                else {
                    panic!("{name} must be an agent_turn");
                };
                *prompt_file = base_file.clone();
            }
            assert_eq!(&planned_stage, base_stage, "stage {name} drifted");
        }

        for role in ["coder", "reviewer"] {
            assert_eq!(planned.roles[role], base.roles[role], "role {role} drifted");
        }
    }

    /// #172: the roles that must not change the worktree are read-only in the
    /// built-ins, and the coder is not.
    #[test]
    fn builtin_read_only_roles_are_enforced() {
        use crate::adapter::RoleTool;
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let base = load_seeded(&dir, "coding-task.yaml");
        let planned = load_seeded(&dir, "coding-task-planned.yaml");
        for (workflow, role) in [
            (&base, "reviewer"),
            (&planned, "reviewer"),
            (&planned, "planner"),
        ] {
            let def = &workflow.roles[role];
            assert!(def.read_only, "{} {role}", workflow.name);
            assert_eq!(def.disallowed_tools, RoleTool::ALL.to_vec());
        }
        for workflow in [&base, &planned] {
            let coder = &workflow.roles["coder"];
            assert!(!coder.read_only);
            assert!(coder.disallowed_tools.is_empty());
        }
    }

    /// Drift guard: each `-planned` prompt is its original with the one
    /// `{{ task.input }}` replaced.
    #[test]
    fn planned_prompts_are_their_originals_with_the_spec_swapped_in() {
        for stem in ["coder-turn", "coder-revise", "reviewer-turn"] {
            let original = embedded_prompt(&format!("{stem}.md"));
            let planned = embedded_prompt(&format!("{stem}-planned.md"));
            assert_eq!(original.matches("{{ task.input }}").count(), 1, "{stem}");
            assert_eq!(
                planned,
                original.replace("{{ task.input }}", "{{ stages.spec_check.summary }}"),
                "{stem}-planned.md drifted"
            );
            assert!(!planned.contains("{{ task.input }}"), "{stem}");
        }
    }

    #[test]
    fn planned_prompts_render_the_spec_not_the_task_text() {
        let payload = serde_json::json!({
            "task": { "title": "T", "input": "ORIGINAL TASK TEXT" },
            "arrival": { "from": "internal_review", "outcome": "changes_requested" },
            "stages": {
                "spec_check": { "summary": "HARDENED SPEC TEXT" },
                "internal_review": { "summary": "R", "outcome": "changes_requested" },
                "escalate_to_human": "H",
            },
        });
        for name in [
            "coder-turn-planned.md",
            "coder-revise-planned.md",
            "reviewer-turn-planned.md",
        ] {
            let (out, _) = crate::template::render(embedded_prompt(name), &payload)
                .unwrap_or_else(|e| panic!("{name} failed to render: {e}"));
            assert!(out.contains("HARDENED SPEC TEXT"), "{name}");
            assert!(!out.contains("ORIGINAL TASK TEXT"), "{name}");
        }
    }

    #[test]
    fn planner_turn_renders_first_turn_and_resumed_turn() {
        let first = serde_json::json!({
            "task": { "title": "T", "input": "THE TASK INPUT" },
            "arrival": { "from": "", "outcome": "" },
        });
        let (out, _) = crate::template::render(embedded_prompt("planner-turn.md"), &first).unwrap();
        assert!(out.contains("THE TASK INPUT"));

        let resumed = serde_json::json!({
            "task": { "title": "T", "input": "THE TASK INPUT" },
            "arrival": { "from": "spec_questions", "outcome": "resumed" },
            "stages": {
                "spec_check": { "summary": "PREVIOUS REPORT" },
                "spec_questions": "HUMAN ANSWER",
            },
        });
        let (out, _) =
            crate::template::render(embedded_prompt("planner-turn.md"), &resumed).unwrap();
        assert_eq!(out.matches("PREVIOUS REPORT").count(), 1);
        assert_eq!(out.matches("HUMAN ANSWER").count(), 1);
        let prev_heading = out.find("## Your previous report").unwrap();
        let answer_heading = out.find("## The human's answer").unwrap();
        let prev = out.find("PREVIOUS REPORT").unwrap();
        let answer = out.find("HUMAN ANSWER").unwrap();
        assert!(prev_heading < prev && prev < answer_heading);
        assert!(answer_heading < answer);
    }

    /// The text between `heading` and the next `## ` heading, squashed.
    fn section_of(text: &str, heading: &str) -> String {
        let (_, after) = text
            .split_once(heading)
            .unwrap_or_else(|| panic!("missing {heading:?}"));
        let body = after.split("\n## ").next().unwrap();
        squash(body)
    }

    #[test]
    fn planner_system_pins_the_stop_criteria() {
        let section = section_of(
            embedded_prompt("planner-system.md"),
            "## Your job, and when to stop",
        );
        let numbered = section
            .split(' ')
            .filter(|w| w.len() == 2 && w.ends_with('.') && w.as_bytes()[0].is_ascii_digit())
            .collect::<Vec<_>>();
        assert_eq!(numbered, ["1.", "2.", "3."]);
        assert_says(
            &section,
            &[
                "contradicts itself about what is wanted",
                "The goal is missing or ambiguous",
                "irreversible",
                "security-relevant",
                "clearly costly",
                "Nothing else is a reason to stop",
            ],
            "planner-system.md stop criteria",
        );
    }

    #[test]
    fn planner_system_names_the_sections_the_stage_enforces_in_order() {
        use crate::workflow_def::StageKind;
        let dir = TempDir::new();
        seed_builtin_workflows(&dir.path).unwrap();
        let def = load_seeded(&dir, "coding-task-planned.yaml");
        let StageKind::AgentTurn {
            report_sections, ..
        } = &def.stages["spec_check"].kind
        else {
            panic!("spec_check should be an agent_turn");
        };
        assert!(!report_sections.is_empty());
        let section = section_of(embedded_prompt("planner-system.md"), "## What you report");
        let mut from = 0;
        for name in report_sections {
            let found = section[from..]
                .find(&format!("**{name}.**"))
                .unwrap_or_else(|| panic!("'{name}' missing or out of order: {section}"));
            from += found + name.len();
        }
    }

    fn index_of(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("expected \"{needle}\" in:\n{text}"))
    }

    #[test]
    fn coder_system_asks_for_the_self_check() {
        let text = squash(embedded_prompt("coder-system.md"));
        let checks = [
            "break the line, run that one test, see it fail, restore the line",
            "\"Hard to trigger\", \"documented\" and \"known gap\" are not reasons.",
            "A blocking finding is fixed, not documented.",
            "To dispute one, show it from the code.",
            "A reviewer's suggested fix is a hint, not a spec.",
            "Restore it with `git checkout -- <file>`, never by hand, and never before the work is committed",
            "Commit your work first, so the tree is clean and a break can't be mistaken for your changes.",
            "Commit a new test before you break the code it covers.",
            "Then confirm `git status --short` is empty, re-run the test and see it pass.",
            "When the self-check ends, the tree must be exactly what you meant to commit, and the tests must prove it.",
            "A branch no test can reach needs the code fact that shows it;",
            "one on the change's main path that you would list as untested, or as covered only by a unit test of a helper, isn't finished until it has its test or that code fact, and the reviewer blocks on it.",
            "Before you build on anything the spec marks **unverified**, run its probe where the claim applies, and give the command and its result in the PR description under `## Look closely at`.",
            "If the result contradicts the spec, adapt as little as possible and say what changed there.",
            "If the probe can't run where the claim applies, say there why it couldn't, in place of its result.",
        ];
        assert_says(
            &text,
            &checks,
            "coder-system.md must ask for the self-check",
        );
        let step2 = index_of(&text, "2. When you change code");
        let step3 = index_of(&text, "3. Commit everything");
        for c in checks {
            let at = index_of(&text, c);
            assert!(step2 < at && at < step3, "'{c}' must sit in step 2");
        }
        let step2_text = &text[step2..step3];
        assert_eq!(step2_text.matches("isn't finished").count(), 1);
        assert_eq!(step2_text.matches("the code fact that shows it").count(), 1);
        assert!(!step2_text.contains("Listing it is not finishing it"));
        let step4 = index_of(&text, "4. Write the pull request's description");
        let step5 = index_of(&text, "5. Call `report_outcome`");
        let claims = [
            "Don't claim a test pins something unless you ran that test against the broken code.",
            "Don't call something untestable.",
        ];
        assert_says(&text, &claims, "coder-system.md step 4 must limit claims");
        for c in claims {
            let at = index_of(&text, c);
            assert!(step4 < at && at < step5, "'{c}' must sit in step 4");
        }
    }

    #[test]
    fn coder_revise_applies_the_self_check_to_the_lap() {
        let raw = embedded_prompt("coder-revise.md");
        let done = &raw[raw.find("## When you're done").expect("section")..];
        assert_says(
            &squash(done),
            &[
                "The self-check in step 2 of your instructions applies to every branch this lap \
               added or changed, including new message text and new tests.",
                "Commit the lap's work before you run it, restore each break with \
               `git checkout -- <file>`, and confirm `git status --short` \
               is empty and the test passes again before you finish.",
            ],
            "coder-revise.md must apply the self-check to the lap",
        );
        for name in ["coder-revise.md", "coder-revise-planned.md"] {
            let raw = embedded_prompt(name);
            let done = squash(&raw[raw.find("## When you're done").expect("section")..]);
            assert_says(
                &done,
                &[
                    "Keep every section the spec requires.",
                    "If the branch has an open PR, first read its published description: set `N` as the `awaiting_human_review` entry does, then `[ -n \"$N\" ] && gh pr view \"$N\" --json body -q .body`.",
                    "Only the part between the issue line and `## Internal review` is your description; carry into the file every edit a person made there that is still true.",
                ],
                name,
            );
            assert!(
                !done.contains("Re-read the whole description, delete what is no longer true"),
                "{name}: the old re-read sentence is gone"
            );
        }
    }

    #[test]
    fn prompts_wait_on_their_own_job_not_pgrep() {
        let phrases = [
            "Wait by ending the turn, not by polling",
            "Never wait on `pgrep` for a program name",
            "never check a pid or loop until a line appears that the job may never print",
        ];
        let coder = squash(embedded_prompt("coder-system.md"));
        assert_says(&coder, &phrases, "coder-system.md wait rule");
        let (from, to) = (
            index_of(&coder, "1. Do the work yourself"),
            index_of(&coder, "2. When you change code"),
        );
        for p in phrases {
            let at = index_of(&coder, p);
            assert!(from < at && at < to, "'{p}' must sit in coder step 1");
        }
        assert!(!coder.contains("kill -0"), "no pid polling in coder step 1");
        let reviewer = squash(embedded_prompt("reviewer-system.md"));
        assert_says(&reviewer, &phrases, "reviewer-system.md wait rule");
        assert!(
            !reviewer.contains("kill -0"),
            "no pid polling for the reviewer"
        );
        let (from, to) = (
            index_of(&reviewer, "Wait for that work before you report."),
            index_of(&reviewer, "If something outside the code stops you"),
        );
        for p in phrases {
            let at = index_of(&reviewer, p);
            assert!(
                from < at && at < to,
                "'{p}' must sit in the waiting paragraph"
            );
        }
    }

    #[test]
    fn planner_system_has_a_sixth_soundness_check() {
        let raw = embedded_prompt("planner-system.md");
        let start = raw.find("## The checks").expect("checks section");
        let rest = &raw[start + "## The checks".len()..];
        let section = &rest[..rest.find("\n## ").unwrap_or(rest.len())];
        let numbered: Vec<&str> = section
            .lines()
            .map(str::trim_start)
            .filter(|l| {
                let digits = l.chars().take_while(|c| c.is_ascii_digit()).count();
                digits > 0 && l[digits..].starts_with(". **")
            })
            .collect();
        let numbers: Vec<&str> = numbered
            .iter()
            .map(|l| &l[..l.find('.').unwrap()])
            .collect();
        assert_eq!(numbers, ["1", "2", "3", "4", "5", "6"]);
        assert!(numbered[5].starts_with("6. **Soundness.**"), "{numbered:?}");
        let from = section.find("4. **Decidedness.**").unwrap();
        let to = section.find("5. **Testability.**").unwrap();
        let fourth = squash(&section[from..to]);
        assert_says(
            &fourth,
            &[
                "When a stated decision leaves the protection unsound, the task contradicts itself: treat it as stop condition 1 and raise it as a question instead of keeping the decision silently.",
                "Every runtime claim the spec relies on (what a program prints, writes, returns or includes), whether a Decision states it, a Decision depends on it without stating it, or it is carried over from the task or the issue, cites the command you ran and its output.",
                "If the rule above doesn't let you run it, mark the claim **unverified** and name the probe the coder must run before relying on it.",
                "Run the probe where the claim applies (the same tool, harness and kind of process the claim is about), not in a stand-in:",
                "a run in a stand-in (a plain shell for a claim about an agent harness) doesn't verify the claim; mark it **unverified**.",
            ],
            "planner-system.md check 4",
        );
        let sixth = squash(&section[section.find("6. **Soundness.**").unwrap()..]);
        assert_says(
            &sixth,
            &[
                "list every way the protected event can end: each exit path, the daemon's restart sweep, a resumed retry and a fresh one.",
                "Check that \"Do not build\" doesn't forbid a hook that list needs.",
                "Require one test that renders every fixture at every size from the minimum up and asserts the invariants.",
                "The done criteria name one test per fail-closed path they mention.",
                "Like the other checks, you fix what you find yourself. A soundness gap is a reason to stop only under the stop conditions above.",
                "When the spec filters or matches on a value (an outcome name, a status, a character set, a check state), grep every place that writes that field",
                "Say which values the filter handles and why the others don't matter",
                "Include the state the world may already be in when the operation starts (an existing branch, file, row or remote commit), not only races during it.",
                "A fix the task decides goes through this check too; if it fails, that is check 4's contradiction: raise it as a question.",
            ],
            "planner-system.md check 6",
        );
        assert!(
            index_of(&sixth, "Include the state the world may already be in")
                < index_of(
                    &sixth,
                    "Like the other checks, you fix what you find yourself"
                ),
            "the 'Like the other checks' bullet must stay last"
        );
    }

    #[test]
    fn coder_system_step_four_writes_the_description_whole() {
        let text = squash(embedded_prompt("coder-system.md"));
        let step = &text[index_of(&text, "4. Write the pull request's description")
            ..index_of(&text, "5. Call `report_outcome`")];
        assert_says(
            step,
            &[
                "Write the file whole, every turn, from a complete draft that starts from the file's current text (if there is one), with your file-writing tool or a quoted heredoc.",
                "Never change part of it by searching for a heading or any other string (a find-and-replace edit, or a script that splices around a heading): the string can match inside quoted text or code and cut what follows.",
                "To keep a person's edits from the published description, copy them into your draft, which you then write whole.",
                "After writing it, read the file back and check that every section is there and ends where you meant it to.",
                "If the write fails, or the read-back still shows a cut after you rewrite it, say so in the `report_outcome` summary, under what you didn't do.",
            ],
            "coder-system.md step 4",
        );
    }

    #[test]
    fn coder_revise_rewrites_the_description_file_whole() {
        let raw = embedded_prompt("coder-revise.md");
        let done = squash(&raw[index_of(raw, "## When you're done")..]);
        assert_says(
            &done,
            &[
                "Commit your revisions, and rewrite the PR description file whole (step 4 of your instructions) so it describes the branch as it now stands.",
                "done by rewriting that file",
                "Start your draft from the file's current text, change what this lap changes, and write the whole result in one write; never append to it, find-and-replace in it, or splice around a heading.",
            ],
            "coder-revise.md closing section",
        );
        assert!(!done.contains("update the PR description file"));
        assert!(!done.contains("done by editing that file"));
    }

    #[test]
    fn reviewer_turn_blocks_on_a_cut_description() {
        let text = squash(embedded_prompt("reviewer-turn.md"));
        let blocking = "A description that is cut off or truncated, or is missing `## Problem`, `## Solution`, `## Changes, in reading order` or a section the task requires, is a blocking finding: unlike a description that is missing or empty, which the published PR states outright, it reads as whole when it isn't.";
        assert_says(
            &text,
            &[
                blocking,
                "A file longer than 16,384 bytes counts as truncated, because publishing keeps only that much.",
                "To find a cut, compare the file with the diff and, on a re-review, with the published description (`gh pr view --json body -q .body`, the part between the issue line and `## Internal review`): a numbered list that skips items, a section that stops mid-sentence, or a section the earlier version had that is gone though the change didn't remove its subject, is a cut.",
                "`## Look closely at`, `## Review history` and `## Not done` are left out when they would be empty, and their absence is not a cut; a section the task requires is never optional.",
                "If you can't read the published description (no pull request yet, or `gh` fails), check the file alone and name under `Reviewed` what you couldn't compare.",
            ],
            "reviewer-turn.md",
        );
        let list_at = index_of(&text, "These are non-blocking findings:");
        assert!(index_of(&text, blocking) < list_at);
        let list = &text[list_at..];
        let list = &list[..=list.find('.').unwrap()];
        for word in ["cut", "truncated", "missing a section", "required"] {
            assert!(
                !list.contains(word),
                "non-blocking list mentions {word}: {list}"
            );
        }
    }

    #[test]
    fn reviewer_turn_byte_limit_matches_open_pr_cap() {
        let (_, script) = BUILTIN_WORKFLOW_SCRIPTS
            .iter()
            .find(|(name, _)| *name == "open-pr.sh")
            .expect("open-pr.sh is embedded");
        assert!(script.contains("cap 16384 "), "open-pr.sh cap changed");
        assert!(
            squash(embedded_prompt("reviewer-turn.md")).contains("16,384 bytes"),
            "reviewer-turn.md must state the same limit as open-pr.sh"
        );
    }

    #[test]
    fn planner_system_check_six_bullets_stay_inside_the_list_item() {
        let raw = embedded_prompt("planner-system.md");
        let rest = &raw[index_of(raw, "## The checks")..];
        let sixth = &rest[index_of(rest, "6. **Soundness.**")..];
        let end = sixth.find("\n## ").expect("a heading follows check 6");
        let bullets: Vec<&str> = sixth[..end]
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .collect();
        assert!(bullets.len() >= 4, "check 6 has its bullets");
        for b in bullets {
            assert!(b.starts_with("   - "), "bullet not at 3 spaces: {b:?}");
        }
    }

    #[test]
    fn planner_system_check_six_has_counter_and_key_bullets() {
        let raw = embedded_prompt("planner-system.md");
        let rest = &raw[index_of(raw, "## The checks")..];
        let sixth = squash(&rest[index_of(rest, "6. **Soundness.**")..]);
        let counter = "When the spec takes a counter or a running total as its source of truth, list everything that can move it down or reset it (a restart, a retry, a compaction, a rollover) and say how the design handles each.";
        let keys = "When the spec builds a key (an identifier, lookup key or match string) in two places and matches the two across, it says to build it once and share the builder.";
        assert_says(
            &sixth,
            &[
                counter,
                "When nothing can, say so and cite what you grepped or ran.",
                "When the counter belongs to an external tool and the run rule doesn't let you make it drop, mark the claim that it only rises **unverified** and name the probe the coder must run, as in check 4.",
                keys,
                "When the two places can't share code (different languages or processes, or keys an older version already stored), the spec still names one canonical form and requires a test that builds the key on both sides from the same inputs, empty and missing parts included, and asserts they are equal.",
            ],
            "planner-system.md check 6",
        );
        let last = index_of(
            &sixth,
            "Like the other checks, you fix what you find yourself",
        );
        assert!(index_of(&sixth, counter) < last);
        assert!(index_of(&sixth, keys) < last);
    }

    #[test]
    fn planner_system_has_one_run_rule() {
        let raw = embedded_prompt("planner-system.md");
        let head = squash(&raw[..raw.find("## Your job, and when to stop").unwrap()]);
        assert_says(
            &head,
            &[
                "Don't edit any file outside a temporary directory you made.",
                "You may run read-only commands, such as `git fetch`, and harmless runs that exercise a tool without lasting effect: `--help`, `--version`, a dry run, or the command on a throwaway input in a temporary directory.",
                "Never install anything, build, run tests, commit, push or post anywhere.",
            ],
            "planner-system.md run rule",
        );
        let checks = section_of(raw, "## The checks");
        assert_says(
            &checks,
            &[
                "run it as written where the rule above allows that, and otherwise a run the rule allows that exercises the same tool and flags",
            ],
            "planner-system.md check 1",
        );
        let one = index_of(&checks, "1. **Reachability.**");
        let two = index_of(&checks, "2. **Base.**");
        let at = index_of(&checks, "run it as written where the rule above allows");
        assert!(one < at && at < two, "the rule reference sits in check 1");
        for gone in [
            "Run it as written only if it is read-only",
            "Never install anything, build, run tests, commit, push or post to prove a command",
            "can't run it read-only",
        ] {
            assert!(!checks.contains(gone), "{gone:?} is gone");
        }
    }

    #[test]
    fn reviewer_system_checks_unverified_claims_in_conformance() {
        let text = squash(embedded_prompt("reviewer-system.md"));
        let claims = [
            "For each claim the task marks **unverified**, check that the PR description gives its probe and result and that the code fits that result; re-run the probe when it is harmless to.",
            "A probe that could not run where the claim applies is not missing if the PR description says why;",
            "a missing probe (neither a result nor that reason), or code built on a claim the probe contradicted, is a blocking finding.",
        ];
        assert_says(&text, &claims, "reviewer-system.md step 4");
        let (from, to) = (
            index_of(&text, "## 4. Conformance"),
            index_of(&text, "## 5. Decide"),
        );
        for c in claims {
            let at = index_of(&text, c);
            assert!(from < at && at < to, "'{c}' must sit in step 4");
        }
    }

    #[test]
    fn prompts_read_issues_and_prs_in_a_form_that_prints() {
        for (name, source) in BUILTIN_WORKFLOW_PROMPTS {
            assert!(
                !squash(source).contains("--comments"),
                "{name} still uses --comments"
            );
        }
        assert_says(
            &squash(embedded_prompt("planner-system.md")),
            &["gh issue view 12 --json title,body,comments"],
            "planner-system.md",
        );
        for name in [
            "reviewer-turn.md",
            "reviewer-turn-planned.md",
            "coder-revise.md",
            "coder-revise-planned.md",
        ] {
            assert_says(
                &squash(embedded_prompt(name)),
                &["gh pr view --json comments,reviews"],
                name,
            );
        }
    }

    #[test]
    fn reviewer_system_runs_the_repos_gate_before_approving() {
        let text = squash(embedded_prompt("reviewer-system.md"));
        let clauses = [
            "Before you report `approved`, run every command the repository's own instruction files (CLAUDE.md, AGENTS.md, CONTRIBUTING) say a change must pass, exactly as they state them (or, if none of them names one, the CI configuration's checks; if nothing names a gate, say so under `Reviewed`), in your scratch copy reset to HEAD with the reset command above.",
            "A command that fails is a blocking finding; quote its failing output.",
            "A failing test the diff doesn't edit, whose failure output doesn't point at code the change edited, gets one re-run on its own, in your scratch copy: it blocks only if it fails again.",
            "Name it under `Reviewed` either way, with both results.",
            "Approve only after a gate that ran to the end: every command ran in full or couldn't start for a reason outside the change, and the only failures were tests whose single re-run passed.",
            "If a failure stopped a command early, run what it skipped, in your scratch copy, before you approve.",
            "One that can't start here for a reason outside the change (a tool not installed, no network) is named with its error and doesn't block by itself.",
            "Record each command and its result under `Reviewed`.",
            "A review that already rejects skips this: the next review runs it.",
            "Never run the gate, a build, tests or a formatter in the task worktree, whatever verdict you expect: the scratch copy is the only place, and skipping is the only alternative.",
        ];
        assert_says(&text, &clauses, "reviewer-system.md gate");
        let predict = index_of(&text, "## 1. Predict");
        for c in clauses {
            assert!(index_of(&text, c) < predict, "'{c}' must sit before step 1");
        }
        assert!(!text.contains("Don't re-run formatting, lint, build or the full test suite"));
    }

    #[test]
    fn reviewer_system_calibrates_severity() {
        let text = squash(embedded_prompt("reviewer-system.md"));
        assert_says(
            &text,
            &[
                "A branch that only chooses message text (same state written, same routing, same stop) is a minor finding.",
                "in either of two cases",
                "such as calling a retry safe when it isn't",
                "An untested branch on the path the change is mainly about blocks",
                "follow the way out (a fresh retry and a resumed one) and check that the protection still holds after it",
                "carried minors: N, see report of <sha>",
            ],
            "reviewer-system.md severity calibration",
        );
        assert!(
            index_of(&text, "## 5. Decide")
                < index_of(&text, "carried minors: N, see report of <sha>"),
            "the carried-minors rule belongs in step 5"
        );
    }

    #[test]
    fn reviewer_turn_weighs_pr_claims_both_ways() {
        let text = squash(embedded_prompt("reviewer-turn.md"));
        assert_says(
            &text,
            &[
                "A false claim stays blocking when it overstates what the code does or protects.",
                "already a finding: one finding, not two.",
                "or show that the branch only chooses message text that wouldn't lead the operator to a wrong action.",
                "Minor findings carried unchanged collapse into one line",
            ],
            "reviewer-turn.md PR claims and item 1",
        );
    }
}
