//! Shared traversal guard for a path referenced from a trusted-ish file and
//! resolved relative to that file's own directory — the workflow loader's
//! `prompt_file`/`system_prompt_file`/`script_file` references and global
//! config's `system_prompt_file` (P1-8 LLD §2.2/§2.4) both need exactly
//! this check; this module exists so there's exactly one implementation of
//! it, not two copies that could drift apart.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum FileRefError {
    /// `relative` was absolute or contained a `..` component.
    Escapes,
    /// The joined path doesn't exist on disk.
    Missing(PathBuf),
}

/// Joins `relative` onto `base_dir`, rejecting absolute paths and `..`
/// components up front — without this, a reference could walk straight out
/// of `base_dir` (e.g. `/etc/passwd` or `../../../../etc/passwd`) — then
/// fails if the resolved path doesn't exist, so a typo'd reference is
/// caught at load time rather than deep inside a running task.
pub fn resolve_relative(base_dir: &Path, relative: &str) -> Result<PathBuf, FileRefError> {
    let rel_path = Path::new(relative);
    let escapes = rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if escapes {
        return Err(FileRefError::Escapes);
    }

    let resolved = base_dir.join(rel_path);
    if !resolved.is_file() {
        return Err(FileRefError::Missing(resolved));
    }
    Ok(resolved)
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
                "chokofactoryd-fileref-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }

        fn write(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::write(&path, "content").unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn resolves_an_existing_relative_file() {
        let dir = TempDir::new();
        let expected = dir.write("prompt.md");
        let resolved = resolve_relative(&dir.path, "prompt.md").unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn rejects_an_absolute_path() {
        let dir = TempDir::new();
        let err = resolve_relative(&dir.path, "/etc/passwd").unwrap_err();
        assert!(matches!(err, FileRefError::Escapes));
    }

    #[test]
    fn rejects_a_path_with_a_parent_dir_component() {
        let dir = TempDir::new();
        let err = resolve_relative(&dir.path, "../../../../etc/passwd").unwrap_err();
        assert!(matches!(err, FileRefError::Escapes));
    }

    #[test]
    fn reports_a_missing_file_distinctly_from_an_escaping_path() {
        let dir = TempDir::new();
        let err = resolve_relative(&dir.path, "does-not-exist.md").unwrap_err();
        assert!(matches!(err, FileRefError::Missing(_)));
    }
}
