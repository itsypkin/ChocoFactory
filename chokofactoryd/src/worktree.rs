//! Manages the git worktree lifecycle for coding-style tasks (design §5.5,
//! Q7): a working copy is created on first entry into a stage that needs
//! one and removed once the task reaches a terminal stage or is cancelled.
//! This is the standalone git plumbing; the workflow engine (#7) is what
//! will eventually call `ensure`/`remove` at the right stage transitions.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};

use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug)]
pub enum WorktreeError {
    Spawn(std::io::Error),
    /// A filesystem operation other than spawning git (reading a `.git`
    /// gitlink, writing the completion marker, canonicalizing a path).
    Io(std::io::Error),
    NotAGitRepo {
        path: PathBuf,
        source: Box<WorktreeError>,
    },
    NoParentDir(PathBuf),
    /// `worktree_path` resolves relative to `repo.parent()`; a relative
    /// `repo` would make that resolution depend on the daemon's current
    /// working directory, which can silently vary across calls.
    RepoNotAbsolute(PathBuf),
    /// `kind` names which identifier was rejected ("project" or "task_id").
    InvalidIdentifier {
        kind: &'static str,
        value: String,
    },
    /// `project` and `task_id` each pass `validate_identifier` on their
    /// own, but joined via `-wt-` they'd produce a path component longer
    /// than `MAX_WORKTREE_DIR_LEN`.
    CombinedIdentifierTooLong {
        project: String,
        task_id: String,
        len: usize,
    },
    /// Something already exists at the computed worktree path that isn't
    /// a *complete, verified* git worktree of the requested `repo` — no
    /// `.git` gitlink at all, a checkout interrupted mid-`add` (gitlink
    /// present but our completion marker isn't), or a worktree that
    /// actually belongs to some other repository.
    PathOccupied(PathBuf),
    GitFailed {
        args: Vec<String>,
        stderr: String,
    },
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::Spawn(err) => write!(f, "failed to spawn git: {err}"),
            WorktreeError::Io(err) => write!(f, "filesystem error: {err}"),
            WorktreeError::NotAGitRepo { path, source } => {
                write!(f, "not a git repository: {} ({source})", path.display())
            }
            WorktreeError::NoParentDir(path) => write!(
                f,
                "repo path has no parent directory to place a worktree next to: {}",
                path.display()
            ),
            WorktreeError::RepoNotAbsolute(path) => {
                write!(f, "repo path must be absolute: {}", path.display())
            }
            WorktreeError::InvalidIdentifier { kind, value } => {
                write!(
                    f,
                    "invalid {kind} {value:?}: must be non-empty, at most {MAX_IDENTIFIER_LEN} bytes, must not be \".\"/\"..\" or contain \"-wt-\", and may only contain ASCII letters, digits, '-', '_', or '.'"
                )
            }
            WorktreeError::CombinedIdentifierTooLong {
                project,
                task_id,
                len,
            } => write!(
                f,
                "project {project:?} and task_id {task_id:?} combine into a {len}-byte worktree directory name, exceeding the {MAX_WORKTREE_DIR_LEN}-byte filesystem limit"
            ),
            WorktreeError::PathOccupied(path) => write!(
                f,
                "worktree path {} already exists but isn't a complete, verified git worktree of this repo — a previous `ensure` may have been interrupted, the path may belong to a different repo, or it's used by something else; remove it manually before retrying",
                path.display()
            ),
            WorktreeError::GitFailed { args, stderr } => {
                let rendered = args
                    .iter()
                    .map(|a| format!("{a:?}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                write!(f, "git {rendered} failed: {}", stderr.trim())
            }
        }
    }
}

impl std::error::Error for WorktreeError {}

/// Per-worktree-path locks serializing `ensure`/`remove` so concurrent
/// calls for the same task don't race on the `path.exists()` check
/// followed by a non-atomic git operation. Mirrors the per-key-lock
/// convention in `session.rs`'s `Mutex<HashMap<String, SessionSlot>>`.
/// Entries are removed once nobody else is waiting on them, so this map
/// doesn't grow unbounded over the daemon's lifetime.
///
/// Keyed by the resolved worktree path rather than the raw `(project,
/// task_id)` pair: `worktree_path` joins them with a plain `-wt-`
/// separator, which isn't collision-free (e.g. `("foo", "bar-wt-baz")`
/// and `("foo-wt-bar", "baz")` both resolve to `foo-wt-bar-wt-baz`).
/// Keying on the path guarantees two pairs that resolve to the same
/// directory always share the same lock, since that directory — not the
/// identifier pair — is the actual contended resource.
static LOCKS: LazyLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn lock_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn acquire_lock(key: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = LOCKS.lock().unwrap();
    locks
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// Drops the map entry for `key` if `lock` is the last reference to it
/// (i.e. the map's own clone plus ours, and nobody else concurrently
/// waiting) — otherwise a still-waiting caller would create a fresh,
/// disconnected lock instead of joining the existing queue.
fn release_lock(key: &str, lock: &Arc<AsyncMutex<()>>) {
    let mut locks = LOCKS.lock().unwrap();
    if Arc::strong_count(lock) <= 2 {
        locks.remove(key);
    }
}

/// Holds the per-worktree-path lock for the duration of an
/// `ensure`/`remove` call. An RAII guard (rather than a manual
/// lock/unlock pair) so the `LOCKS` entry is reliably released via
/// `Drop` on every exit path — normal return, an error via `?`, or the
/// calling future being dropped mid-await (e.g. under a
/// `tokio::time::timeout`) — without which a cancelled caller could
/// otherwise leak that entry for the rest of the process's lifetime.
struct KeyLock {
    key: String,
    lock: Arc<AsyncMutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl KeyLock {
    async fn acquire(path: &Path) -> Self {
        let key = lock_key(path);
        let lock = acquire_lock(&key);
        // Construct `Self` (with `guard: None`) *before* awaiting the
        // mutex, not after. If this function's caller is cancelled while
        // still waiting to acquire — before the mutex is ever held — `self`
        // already exists as a live local in this async fn's state machine,
        // so `Drop` still runs and `release_lock` still fires. Building
        // `Self` only once the guard is in hand would skip `Drop`
        // entirely for a cancellation in that window, leaking the map
        // entry for `key` (self-healing only if that same key is ever
        // used again — never, otherwise).
        let mut this = Self {
            key,
            lock: Arc::clone(&lock),
            guard: None,
        };
        this.guard = Some(lock.lock_owned().await);
        this
    }
}

impl Drop for KeyLock {
    fn drop(&mut self) {
        // Release the mutex itself first so `release_lock`'s strong-count
        // check reflects only the map's clone and `self.lock`, not also
        // the owned guard's internal clone.
        self.guard.take();
        release_lock(&self.key, &self.lock);
    }
}

/// A generous per-identifier sanity cap. Doesn't by itself bound the
/// combined `{project}-wt-{task_id}` path component — see
/// `MAX_WORKTREE_DIR_LEN` for that.
const MAX_IDENTIFIER_LEN: usize = 200;

/// POSIX `NAME_MAX` on most Linux/macOS filesystems: the length limit for
/// a single path component, which `worktree_path` builds as
/// `{project}-wt-{task_id}`. Two identifiers each under
/// `MAX_IDENTIFIER_LEN` can still combine past this, so it's checked
/// separately in `worktree_path`.
const MAX_WORKTREE_DIR_LEN: usize = 255;

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), WorktreeError> {
    let is_valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && value != "."
        && value != ".."
        // `worktree_path` joins `project`/`task_id` with a literal
        // `-wt-`; banning that substring from either part prevents the
        // documented collision (e.g. ("foo", "bar-wt-baz") vs.
        // ("foo-wt-bar", "baz")) from silently aliasing two unrelated
        // tasks onto the same checkout/branch.
        && !value.contains("-wt-")
        // Allowlist rather than denylist: fails fast with a clear
        // `InvalidIdentifier` for anything unusual (spaces, newlines,
        // other punctuation), instead of it sailing through only to hit
        // an opaque `GitFailed` later when git itself rejects it as a
        // ref/path component. Also subsumes the `/`, `\`, NUL checks.
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if is_valid {
        Ok(())
    } else {
        Err(WorktreeError::InvalidIdentifier {
            kind,
            value: value.to_string(),
        })
    }
}

/// The sibling directory a task's worktree lives in, per design §5.5:
/// `../<project>-wt-<task_id>` relative to `repo`.
pub fn worktree_path(repo: &Path, project: &str, task_id: &str) -> Result<PathBuf, WorktreeError> {
    if !repo.is_absolute() {
        return Err(WorktreeError::RepoNotAbsolute(repo.to_path_buf()));
    }
    validate_identifier("project", project)?;
    validate_identifier("task_id", task_id)?;
    let dir_name = format!("{project}-wt-{task_id}");
    if dir_name.len() > MAX_WORKTREE_DIR_LEN {
        return Err(WorktreeError::CombinedIdentifierTooLong {
            project: project.to_string(),
            task_id: task_id.to_string(),
            len: dir_name.len(),
        });
    }
    let parent = repo
        .parent()
        .ok_or_else(|| WorktreeError::NoParentDir(repo.to_path_buf()))?;
    Ok(parent.join(dir_name))
}

/// The branch a task's worktree checks out, per design §5.5: `task/<task_id>`.
///
/// Note: this depends only on `task_id`, not `project` — unlike
/// `worktree_path` (and the `KeyLock` keyed on it), the branch name and
/// the per-path lock don't cover the same identity. Callers must treat
/// `task_id` as unique across every `(repo, project)` this module ever
/// manages, not just within one project: two `ensure` calls for the same
/// `repo` with the *same* `task_id` but *different* `project` strings
/// would resolve to different worktree paths (so different locks, no
/// mutual exclusion) while racing to create the identical branch
/// `task/<task_id>` in that repo.
pub fn branch_name(task_id: &str) -> String {
    format!("task/{task_id}")
}

/// Filename of the marker `ensure_locked` writes into a worktree's git
/// admin directory (`<repo>/.git/worktrees/<name>/`, *not* the checked-out
/// working directory itself) once `git worktree add` has fully finished.
/// Its presence is what lets the `path.exists()` fast path distinguish a
/// genuinely complete worktree from one left mid-checkout by a crash (git
/// writes the `.git` gitlink before populating files, so gitlink presence
/// alone doesn't prove the checkout finished) or from an unrelated
/// directory.
const COMPLETE_MARKER: &str = "chokofactory-complete";

/// Reads a linked worktree's `.git` gitlink file (`gitdir: <admin-dir>`)
/// and returns the admin directory it points at, or `None` if `path`
/// doesn't have one (not a worktree, or corrupt).
fn worktree_admin_dir(path: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(path.join(".git")).ok()?;
    let gitdir = contents.trim().strip_prefix("gitdir:")?.trim();
    Some(PathBuf::from(gitdir))
}

/// Returns the worktree's admin directory only if `path` is a *complete*
/// linked worktree that actually belongs to `repo` — its gitlink resolves
/// to a directory under `repo`'s own common git-dir's `worktrees/`
/// subdirectory (see `git_common_dir` for why that's resolved via git
/// itself rather than assumed to be `<repo>/.git`), and the
/// `COMPLETE_MARKER` written by a prior successful `ensure` is present.
/// Anything else (bogus directory, a worktree of some *other* repo, or a
/// checkout interrupted mid-`add`) returns `None`.
///
/// Note: a `canonicalize`/`git_common_dir` failure for a reason other
/// than "doesn't exist" (e.g. a permissions error) is indistinguishable
/// here from a genuine ownership mismatch — both just fall through to
/// `None`/`PathOccupied`. That's a minor loss of error-message precision,
/// not a correctness issue, since either way the right response is the
/// same: don't treat `path` as ensured.
async fn verified_worktree_admin_dir(repo: &Path, path: &Path) -> Option<PathBuf> {
    let admin_dir = worktree_admin_dir(path)?;
    let admin_parent = admin_dir.parent()?;
    // `git_common_dir` already canonicalizes its result, and git never
    // makes `worktrees/` itself a symlink, so `expected_parent` is already
    // canonical — only `admin_parent` (from the gitlink's raw text) needs
    // resolving before comparing.
    let common_dir = git_common_dir(repo).await.ok()?;
    let expected_parent = common_dir.join("worktrees");
    let actual = std::fs::canonicalize(admin_parent).ok()?;
    if actual != expected_parent {
        return None;
    }
    admin_dir
        .join(COMPLETE_MARKER)
        .exists()
        .then_some(admin_dir)
}

/// Marks a just-created worktree at `path` as complete by writing
/// `COMPLETE_MARKER` into its admin directory. Called only right after
/// `git worktree add` itself has already reported success, so failure
/// here means a real (if unlikely) filesystem problem, not a git one.
///
/// If this fails (or something deletes the marker afterwards — e.g. a
/// concurrent external process touching `.git`, or a transient I/O
/// error), the worktree itself is left fully valid on disk, but every
/// future `ensure` for this task will see the marker missing and
/// permanently return `PathOccupied`, requiring the manual cleanup its
/// error message describes. Deliberately fail-safe rather than fail-open:
/// an occasional false-positive lockout is preferable to silently
/// re-accepting a checkout we can no longer vouch for.
fn mark_worktree_complete(path: &Path) -> Result<(), WorktreeError> {
    let admin_dir =
        worktree_admin_dir(path).ok_or_else(|| WorktreeError::PathOccupied(path.to_path_buf()))?;
    std::fs::write(admin_dir.join(COMPLETE_MARKER), []).map_err(WorktreeError::Io)
}

async fn branch_exists(repo: &Path, branch: &str) -> bool {
    run_git(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok()
}

/// Creates the task's worktree if it doesn't already exist (idempotent, so
/// re-entering the triggering stage — e.g. after a daemon restart — is
/// safe). Returns the worktree's path either way.
pub async fn ensure(repo: &Path, project: &str, task_id: &str) -> Result<PathBuf, WorktreeError> {
    // Validate first: cheap, and gives a more specific/actionable error
    // than paying for a `git rev-parse` only to still fail afterwards.
    let path = worktree_path(repo, project, task_id)?;
    ensure_git_repo(repo).await?;
    let _lock = KeyLock::acquire(&path).await;
    ensure_locked(repo, task_id, &path).await
}

async fn ensure_locked(repo: &Path, task_id: &str, path: &Path) -> Result<PathBuf, WorktreeError> {
    if path.exists() {
        // Bare `.git`-file presence isn't enough: git writes that gitlink
        // (and registers the worktree) *before* populating the working
        // tree, so a daemon crash mid-`add` leaves a `.git` file pointing
        // at a genuinely incomplete checkout. `verified_worktree_admin_dir`
        // additionally requires our own completion marker (written only
        // after `add` fully succeeds, see `mark_worktree_complete`) and
        // that the worktree actually belongs to `repo` — not some other
        // repository's worktree that happens to occupy this path.
        if verified_worktree_admin_dir(repo, path).await.is_none() {
            return Err(WorktreeError::PathOccupied(path.to_path_buf()));
        }
        return Ok(path.to_path_buf());
    }

    // Clear any stale registration left behind if a previous worktree dir
    // was removed by hand (e.g. after a crash) rather than via `git
    // worktree remove` — otherwise `add` fails with "missing but already
    // registered worktree" before we even get to the branch.
    run_git(repo, &["worktree", "prune"]).await?;

    let branch = branch_name(task_id);
    let path_str = path.to_string_lossy().into_owned();
    // Check whether the branch survived (e.g. from a prior attempt whose
    // worktree dir was since removed by hand) directly, rather than
    // parsing git's (locale-dependent, ambiguous) stderr text.
    //
    // `--` ends option parsing before the positional path/branch args,
    // so a `project`/`task_id` starting with `-` (combined with a
    // relative `repo` whose worktree path ends up with no leading `/`)
    // can't be misparsed by git as a flag.
    if branch_exists(repo, &branch).await {
        run_git(repo, &["worktree", "add", "--", &path_str, &branch]).await?;
    } else {
        run_git(repo, &["worktree", "add", "-b", &branch, "--", &path_str]).await?;
    }
    mark_worktree_complete(path)?;
    Ok(path.to_path_buf())
}

/// Removes the task's worktree, discarding any uncommitted changes in it.
/// A no-op if it's already gone (idempotent — task cancellation may race
/// with a stage that already removed it on reaching `done`).
pub async fn remove(repo: &Path, project: &str, task_id: &str) -> Result<(), WorktreeError> {
    let path = worktree_path(repo, project, task_id)?;
    ensure_git_repo(repo).await?;
    let _lock = KeyLock::acquire(&path).await;
    remove_locked(repo, &path).await
}

async fn remove_locked(repo: &Path, path: &Path) -> Result<(), WorktreeError> {
    if !path.exists() {
        return Ok(());
    }
    let path_str = path.to_string_lossy().into_owned();
    // A single `--force` overrides a dirty working tree, but not one this
    // module (or anything else) has separately `git worktree lock`ed —
    // that needs `--force` twice. Not a concern today since nothing here
    // ever locks a worktree, but worth knowing if that ever changes.
    run_git(repo, &["worktree", "remove", "--force", "--", &path_str]).await
}

async fn ensure_git_repo(repo: &Path) -> Result<(), WorktreeError> {
    match run_git(repo, &["rev-parse", "--git-dir"]).await {
        Ok(()) => Ok(()),
        // `rev-parse --git-dir` ran and reported `repo` isn't inside a
        // git working tree — that's the actual "not a git repo" case.
        Err(err @ WorktreeError::GitFailed { .. }) => Err(WorktreeError::NotAGitRepo {
            path: repo.to_path_buf(),
            source: Box::new(err),
        }),
        // Anything else (e.g. `Spawn` if the `git` binary itself is
        // missing/unexecutable) isn't a repo problem — don't mislabel it.
        Err(other) => Err(other),
    }
}

/// Resolves `repo`'s actual common git directory via `git rev-parse
/// --git-common-dir`, rather than assuming the `<repo>/.git`-as-directory
/// layout used elsewhere in this file for computing worktree *paths*
/// (unrelated to admin-directory lookups). That assumption breaks for
/// bare repositories (no `.git` at all — the git-dir *is* `repo`),
/// `--separate-git-dir` repositories (`.git` is a gitlink pointing
/// elsewhere, indistinguishable by shape alone from a worktree/submodule
/// gitlink), and `repo` values that are themselves a linked worktree
/// (common-dir correctly resolves to the shared main repo either way).
/// Asking git directly handles all of these uniformly.
async fn git_common_dir(repo: &Path) -> Result<PathBuf, WorktreeError> {
    let raw = run_git_stdout(repo, &["rev-parse", "--git-common-dir"]).await?;
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    std::fs::canonicalize(&absolute).map_err(WorktreeError::Io)
}

async fn run_git(repo: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    run_git_stdout(repo, args).await.map(|_| ())
}

async fn run_git_stdout(repo: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Without this, dropping the `.output()` future (e.g. the calling
        // task cancelled via `KeyLock`'s cancellation path) leaves the
        // spawned git process running in the background, free to keep
        // mutating the worktree after the lock protecting it has already
        // been released. Matches the convention in `adapter/claude.rs`.
        .kill_on_drop(true)
        .output()
        .await
        .map_err(WorktreeError::Spawn)?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
    fn worktree_path_rejects_path_traversal_in_project() {
        let repo = Path::new("/home/user/myrepo");
        let err = worktree_path(repo, "../../etc", "task-123").unwrap_err();
        assert!(matches!(
            err,
            WorktreeError::InvalidIdentifier {
                kind: "project",
                ..
            }
        ));
    }

    #[test]
    fn worktree_path_rejects_slash_in_task_id() {
        let repo = Path::new("/home/user/myrepo");
        let err = worktree_path(repo, "myproject", "task/../../evil").unwrap_err();
        assert!(matches!(
            err,
            WorktreeError::InvalidIdentifier {
                kind: "task_id",
                ..
            }
        ));
    }

    #[test]
    fn worktree_path_rejects_empty_identifiers() {
        let repo = Path::new("/home/user/myrepo");
        assert!(worktree_path(repo, "", "task-123").is_err());
        assert!(worktree_path(repo, "myproject", "").is_err());
    }

    #[test]
    fn worktree_path_rejects_relative_repo() {
        // A relative `repo` would make `worktree_path`'s sibling-directory
        // resolution depend on the daemon's current working directory.
        let err = worktree_path(Path::new("myrepo"), "myproject", "task-123").unwrap_err();
        assert!(matches!(err, WorktreeError::RepoNotAbsolute(_)));
    }

    #[test]
    fn worktree_path_rejects_lone_identifier_over_max_len() {
        let repo = Path::new("/home/user/myrepo");
        let too_long = "a".repeat(MAX_IDENTIFIER_LEN + 1);
        let err = worktree_path(repo, &too_long, "task-123").unwrap_err();
        assert!(matches!(
            err,
            WorktreeError::InvalidIdentifier {
                kind: "project",
                ..
            }
        ));
    }

    #[test]
    fn worktree_path_rejects_disallowed_characters() {
        let repo = Path::new("/home/user/myrepo");
        assert!(worktree_path(repo, "my project", "task-123").is_err());
        assert!(worktree_path(repo, "myproject", "task\n123").is_err());
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
    async fn ensure_errors_when_path_exists_but_is_not_a_git_worktree() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        // Simulate a directory left behind by an interrupted `git
        // worktree add` (or an unrelated directory occupying the path):
        // it exists, but has no `.git`.
        let bogus_path = root.join("myrepo-wt-task-1");
        std::fs::create_dir_all(&bogus_path).unwrap();
        std::fs::write(bogus_path.join("stray.txt"), "not a worktree\n").unwrap();

        let err = ensure(&repo, "myrepo", "task-1").await.unwrap_err();
        assert!(matches!(err, WorktreeError::PathOccupied(p) if p == bogus_path));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_errors_when_worktree_checkout_is_incomplete() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let path = ensure(&repo, "myrepo", "task-1").await.unwrap();

        // Simulate a crash landing between `git worktree add` finishing
        // (which writes the `.git` gitlink before populating files) and
        // our own completion marker being written, by deleting just the
        // marker and leaving everything else as `add` left it.
        let admin_dir = worktree_admin_dir(&path).unwrap();
        std::fs::remove_file(admin_dir.join(COMPLETE_MARKER)).unwrap();

        let err = ensure(&repo, "myrepo", "task-1").await.unwrap_err();
        assert!(matches!(err, WorktreeError::PathOccupied(p) if p == path));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_errors_when_existing_path_belongs_to_a_different_repo() {
        let root = tempdir();
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        init_repo(&repo_a).await;
        init_repo(&repo_b).await;

        // repo_a and repo_b share the same parent, so they compute the
        // identical worktree path for the same (project, task_id) pair.
        // ensure() for repo_a must not accept a worktree that's actually
        // registered under repo_b's admin directory.
        let path_b = ensure(&repo_b, "shared-name", "task-1").await.unwrap();

        let err = ensure(&repo_a, "shared-name", "task-1").await.unwrap_err();
        assert!(matches!(err, WorktreeError::PathOccupied(p) if p == path_b));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_is_idempotent_when_repo_is_itself_a_linked_worktree() {
        let root = tempdir();
        let main_repo = root.join("myrepo");
        init_repo(&main_repo).await;

        // A linked worktree's `.git` is a *file* (gitlink), not a
        // directory. `git_common_dir` resolves through it to the shared
        // main repo's real git-dir either way, so using one as `repo` for
        // a nested task must work correctly and idempotently, not just
        // succeed once and then break (the bug this replaced).
        let nested_repo = ensure(&main_repo, "myrepo", "outer-task").await.unwrap();

        let first = ensure(&nested_repo, "innerproject", "inner-task")
            .await
            .unwrap();
        let second = ensure(&nested_repo, "innerproject", "inner-task")
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!(first.join("README.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_is_idempotent_against_a_bare_repo() {
        let root = tempdir();
        let plain_repo = root.join("plain");
        init_repo(&plain_repo).await;

        // A bare repo has no `.git` at all — the repo directory itself
        // *is* the git-dir. `git_common_dir` must resolve this correctly
        // (rather than assuming `<repo>/.git`) for idempotency to hold.
        let bare_repo = root.join("bare.git");
        git(
            &root,
            &[
                "clone",
                "--bare",
                "-q",
                plain_repo.to_str().unwrap(),
                bare_repo.to_str().unwrap(),
            ],
        )
        .await;

        let first = ensure(&bare_repo, "myproject", "task-1").await.unwrap();
        let second = ensure(&bare_repo, "myproject", "task-1").await.unwrap();
        assert_eq!(first, second);
        assert!(first.join("README.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ensure_is_idempotent_against_a_separate_git_dir_repo() {
        let root = tempdir();
        let git_dir = root.join("elsewhere.git");
        let repo = root.join("myrepo");

        std::fs::create_dir_all(&repo).unwrap();
        git(
            &repo,
            &[
                "init",
                "-q",
                &format!("--separate-git-dir={}", git_dir.to_str().unwrap()),
            ],
        )
        .await;
        git(&repo, &["config", "user.email", "test@example.com"]).await;
        git(&repo, &["config", "user.name", "Test"]).await;
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "."]).await;
        git(&repo, &["commit", "-q", "-m", "init"]).await;

        // `repo`'s `.git` is a gitlink pointing at `git_dir` — the same
        // *shape* as a linked worktree or submodule, but a perfectly
        // ordinary standalone repo. It must not be treated as occupied.
        let first = ensure(&repo, "myproject", "task-1").await.unwrap();
        let second = ensure(&repo, "myproject", "task-1").await.unwrap();
        assert_eq!(first, second);
        assert!(first.join("README.md").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn remove_surfaces_git_error_for_a_directory_that_is_not_a_worktree() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        let bogus_path = root.join("myrepo-wt-task-1");
        std::fs::create_dir_all(&bogus_path).unwrap();

        let err = remove(&repo, "myrepo", "task-1").await.unwrap_err();
        assert!(matches!(err, WorktreeError::GitFailed { .. }));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn concurrent_ensure_calls_for_same_task_do_not_race() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;
        let repo = Arc::new(repo);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let repo = Arc::clone(&repo);
            handles.push(tokio::spawn(async move {
                ensure(&repo, "myrepo", "task-1").await
            }));
        }

        let mut paths = Vec::new();
        for handle in handles {
            paths.push(handle.await.unwrap().unwrap());
        }
        assert!(paths.iter().all(|p| *p == paths[0]));
        assert!(paths[0].join("README.md").exists());

        std::fs::remove_dir_all(root.as_path()).ok();
    }

    #[tokio::test]
    async fn concurrent_ensure_for_different_task_ids_does_not_interfere() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;
        let repo = Arc::new(repo);

        let (a, b) = tokio::join!(
            ensure(&repo, "myrepo", "task-a"),
            ensure(&repo, "myrepo", "task-b"),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_ne!(a, b);
        assert!(a.join("README.md").exists());
        assert!(b.join("README.md").exists());

        std::fs::remove_dir_all(root.as_path()).ok();
    }

    #[tokio::test]
    async fn lock_map_entry_is_removed_after_calls_settle() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;

        // Unique key so this assertion can't be affected by other tests'
        // entries in the process-wide `LOCKS` map running concurrently.
        let path = ensure(&repo, "lockmap-test-project", "lockmap-test-task")
            .await
            .unwrap();
        remove(&repo, "lockmap-test-project", "lockmap-test-task")
            .await
            .unwrap();

        let key = lock_key(&path);
        assert!(!LOCKS.lock().unwrap().contains_key(&key));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn worktree_path_rejects_wt_substring_to_prevent_directory_collisions() {
        // ("foo", "bar-wt-baz") and ("foo-wt-bar", "baz") would otherwise
        // both resolve to the same on-disk directory ("foo-wt-bar-wt-baz")
        // via the `-wt-` join in `worktree_path`, silently aliasing two
        // unrelated tasks onto the same checkout/branch. Banning the
        // separator substring from either part rejects both instead of
        // handing back a path for either.
        let repo = Path::new("/home/user/myrepo");
        assert!(worktree_path(repo, "foo", "bar-wt-baz").is_err());
        assert!(worktree_path(repo, "foo-wt-bar", "baz").is_err());
    }

    #[test]
    fn worktree_path_rejects_combined_identifiers_that_exceed_filesystem_limit() {
        // Individually well within MAX_IDENTIFIER_LEN, but joined via
        // `-wt-` the path component would be 200 + 4 + 200 = 404 bytes,
        // past the typical 255-byte filesystem NAME_MAX.
        let repo = Path::new("/home/user/myrepo");
        let project = "a".repeat(200);
        let task_id = "b".repeat(200);
        let err = worktree_path(repo, &project, &task_id).unwrap_err();
        assert!(matches!(
            err,
            WorktreeError::CombinedIdentifierTooLong { .. }
        ));
    }

    #[tokio::test]
    async fn concurrent_ensure_and_remove_do_not_race() {
        let root = tempdir();
        let repo = root.join("myrepo");
        init_repo(&repo).await;
        ensure(&repo, "myrepo", "task-1").await.unwrap();
        let repo = Arc::new(repo);

        let mut handles = Vec::new();
        for i in 0..6 {
            let repo = Arc::clone(&repo);
            handles.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    ensure(&repo, "myrepo", "task-1").await.map(|_| ())
                } else {
                    remove(&repo, "myrepo", "task-1").await
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        std::fs::remove_dir_all(root.as_path()).ok();
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
        // The underlying git error should be preserved, not discarded.
        match &err {
            WorktreeError::NotAGitRepo { source, .. } => {
                assert!(matches!(source.as_ref(), WorktreeError::GitFailed { .. }));
            }
            other => panic!("expected NotAGitRepo, got {other:?}"),
        }

        std::fs::remove_dir_all(&root).ok();
    }
}
