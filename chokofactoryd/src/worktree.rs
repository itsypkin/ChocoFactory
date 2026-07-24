use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// Manages the git worktree lifecycle for coding-style tasks (design §5.5,
/// Q7): a working copy is created on first entry into a stage that needs
/// one and removed once the task reaches a terminal stage or is cancelled.
/// This is the standalone git plumbing; the workflow engine (#7) is what
/// will eventually call `ensure`/`remove` at the right stage transitions.
#[derive(Debug)]
pub enum WorktreeError {
    Spawn(std::io::Error),
    NotAGitRepo(PathBuf),
    NoParentDir(PathBuf),
    GitFailed { args: Vec<String>, stderr: String },
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::Spawn(err) => write!(f, "failed to spawn git: {err}"),
            WorktreeError::NotAGitRepo(path) => {
                write!(f, "not a git repository: {}", path.display())
            }
            WorktreeError::NoParentDir(path) => write!(
                f,
                "repo path has no parent directory to place a worktree next to: {}",
                path.display()
            ),
            WorktreeError::GitFailed { args, stderr } => {
                write!(f, "git {} failed: {}", args.join(" "), stderr.trim())
            }
        }
    }
}

impl std::error::Error for WorktreeError {}

/// The sibling directory a task's worktree lives in, per design §5.5:
/// `../<project>-wt-<task_id>` relative to `repo`.
pub fn worktree_path(repo: &Path, project: &str, task_id: &str) -> Result<PathBuf, WorktreeError> {
    let parent = repo
        .parent()
        .ok_or_else(|| WorktreeError::NoParentDir(repo.to_path_buf()))?;
    Ok(parent.join(format!("{project}-wt-{task_id}")))
}

/// The branch a task's worktree checks out, per design §5.5: `task/<task_id>`.
pub fn branch_name(task_id: &str) -> String {
    format!("task/{task_id}")
}

/// Creates the task's worktree if it doesn't already exist (idempotent, so
/// re-entering the triggering stage — e.g. after a daemon restart — is
/// safe). Returns the worktree's path either way.
pub async fn ensure(repo: &Path, project: &str, task_id: &str) -> Result<PathBuf, WorktreeError> {
    ensure_git_repo(repo).await?;
    let path = worktree_path(repo, project, task_id)?;
    if path.exists() {
        return Ok(path);
    }

    // Clear any stale registration left behind if a previous worktree dir
    // was removed by hand (e.g. after a crash) rather than via `git
    // worktree remove` — otherwise `add` fails with "missing but already
    // registered worktree" before we even get to the branch.
    run_git(repo, &["worktree", "prune"]).await?;

    let branch = branch_name(task_id);
    let path_str = path.to_string_lossy().into_owned();
    match run_git(repo, &["worktree", "add", &path_str, "-b", &branch]).await {
        Ok(()) => Ok(path),
        // The branch may already exist from a prior attempt whose worktree
        // dir was since removed by hand; reuse it instead of failing.
        Err(WorktreeError::GitFailed { stderr, .. }) if stderr.contains("already exists") => {
            run_git(repo, &["worktree", "add", &path_str, &branch]).await?;
            Ok(path)
        }
        Err(err) => Err(err),
    }
}

/// Removes the task's worktree, discarding any uncommitted changes in it.
/// A no-op if it's already gone (idempotent — task cancellation may race
/// with a stage that already removed it on reaching `done`).
pub async fn remove(repo: &Path, project: &str, task_id: &str) -> Result<(), WorktreeError> {
    let path = worktree_path(repo, project, task_id)?;
    if !path.exists() {
        return Ok(());
    }
    let path_str = path.to_string_lossy().into_owned();
    run_git(repo, &["worktree", "remove", &path_str, "--force"]).await
}

async fn ensure_git_repo(repo: &Path) -> Result<(), WorktreeError> {
    run_git(repo, &["rev-parse", "--git-dir"])
        .await
        .map_err(|_| WorktreeError::NotAGitRepo(repo.to_path_buf()))
}

async fn run_git(repo: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(WorktreeError::Spawn)?;

    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::GitFailed {
            args: args.iter().map(|s| s.to_string()).collect(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    async fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]).await;
        git(dir, &["config", "user.email", "test@example.com"]).await;
        git(dir, &["config", "user.name", "Test"]).await;
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "."]).await;
        git(dir, &["commit", "-q", "-m", "init"]).await;
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chokofactoryd-worktree-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn worktree_path_places_sibling_dir_next_to_repo() {
        let repo = Path::new("/home/user/myrepo");
        let path = worktree_path(repo, "myproject", "task-123").unwrap();
        assert_eq!(path, Path::new("/home/user/myproject-wt-task-123"));
    }

    #[test]
    fn worktree_path_rejects_repo_with_no_parent() {
        let err = worktree_path(Path::new("/"), "myproject", "task-123").unwrap_err();
        assert!(matches!(err, WorktreeError::NoParentDir(_)));
    }

    #[test]
    fn branch_name_uses_task_prefix() {
        assert_eq!(branch_name("task-123"), "task/task-123");
    }

    #[tokio::test]
    async fn ensure_creates_worktree_on_new_branch() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let path = ensure(&repo, "myrepo", "task-1").await.unwrap();
        assert_eq!(path, root.join("myrepo-wt-task-1"));
        assert!(path.join("README.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let first = ensure(&repo, "myrepo", "task-1").await.unwrap();
        let second = ensure(&repo, "myrepo", "task-1").await.unwrap();
        assert_eq!(first, second);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_reuses_branch_if_worktree_dir_was_removed_by_hand() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let path = ensure(&repo, "myrepo", "task-1").await.unwrap();
        // Simulate a crash: the worktree dir is gone but git still thinks
        // it's registered and the branch survives.
        std::fs::remove_dir_all(&path).unwrap();

        let second = ensure(&repo, "myrepo", "task-1").await.unwrap();
        assert_eq!(second, path);
        assert!(path.join("README.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn remove_deletes_worktree_directory() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let path = ensure(&repo, "myrepo", "task-1").await.unwrap();
        assert!(path.exists());

        remove(&repo, "myrepo", "task-1").await.unwrap();
        assert!(!path.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn remove_is_idempotent_when_already_gone() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        remove(&repo, "myrepo", "task-1").await.unwrap();

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn remove_force_discards_uncommitted_changes() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let path = ensure(&repo, "myrepo", "task-1").await.unwrap();
        std::fs::write(path.join("scratch.txt"), "uncommitted\n").unwrap();

        remove(&repo, "myrepo", "task-1").await.unwrap();
        assert!(!path.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_rejects_non_git_repo() {
        let root = tempdir();
        let not_a_repo = root.join("plain-dir");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        let err = ensure(&not_a_repo, "myproject", "task-1")
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::NotAGitRepo(_)));

        std::fs::remove_dir_all(&root).ok();
    }
}
