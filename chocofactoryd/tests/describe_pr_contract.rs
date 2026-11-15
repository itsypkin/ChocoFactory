//! Facts about the `describe_pr` stage that no single module owns: the work
//! report's path command against real git, and the words the docs, comments
//! and skills use about the writer being read-only.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const WORK_REPORT_PATH: &str =
    r#""$(cd "$(git rev-parse --git-dir)" && pwd)/choco-work-report.md""#;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Text with comment markers and line breaks removed, so a sentence wrapped
/// over several lines of YAML or Markdown reads as one.
fn flat(text: &str) -> String {
    text.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .args(["-c", script])
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.test")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.test")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The prompts name the report with one shell expression. Run it in an
/// ordinary repository and in a linked worktree, both under a path with a
/// space in it, write through it, and find the same file through the path
/// git itself reports (`--absolute-git-dir`, an independent source). Files
/// are compared by device and inode, because macOS spells the same directory
/// as both `/var/...` and `/private/var/...`.
#[test]
fn the_work_report_path_reaches_gits_private_dir_in_ordinary_and_linked_repos() {
    for prompt in [
        "workflows/prompts/coder-system.md",
        "workflows/prompts/reviewer-turn.md",
        "workflows/prompts/writer-turn.md",
    ] {
        assert!(flat(&read(prompt)).contains(WORK_REPORT_PATH), "{prompt}");
    }
    let scratch =
        Scratch(std::env::temp_dir().join(format!("choco work report {}", uuid::Uuid::new_v4())));
    let main = scratch.0.join("main repo");
    fs::create_dir_all(&main).unwrap();
    sh(
        &main,
        "git init -q -b main && echo x > f && git add f && git commit -q -m init",
    );
    let linked = scratch.0.join("linked worktree");
    sh(
        &main,
        &format!("git worktree add -q -b task/x '{}'", linked.display()),
    );

    let mut seen = Vec::new();
    for (what, dir) in [("ordinary", &main), ("linked", &linked)] {
        let path = sh(dir, &format!("echo {WORK_REPORT_PATH}"));
        assert!(path.ends_with("/choco-work-report.md"), "{what}: {path}");
        assert!(!path.contains("--path-format"), "{what}: {path}");
        fs::write(&path, format!("report from the {what} repo\n")).unwrap();

        let git_dir = sh(dir, "git rev-parse --absolute-git-dir");
        let independent = Path::new(&git_dir).join("choco-work-report.md");
        let a = fs::metadata(&path).unwrap();
        let b = fs::metadata(&independent).unwrap();
        assert_eq!(
            (a.dev(), a.ino()),
            (b.dev(), b.ino()),
            "{what}: {path} vs {independent:?}"
        );
        assert_eq!(
            fs::read_to_string(&independent).unwrap(),
            format!("report from the {what} repo\n")
        );
        // Private git data, not the work tree: nothing to commit.
        assert_eq!(sh(dir, "git status --short"), "", "{what}");
        seen.push(independent);
    }
    assert_ne!(seen[0], seen[1], "the two repos have separate report files");
    assert!(
        seen[1].to_string_lossy().contains("worktrees"),
        "the linked worktree's private dir: {:?}",
        seen[1]
    );
}

/// Wherever the writer is described as read-only or its failure is
/// discussed, the text says what choco does (detects the change, parks the
/// task), what it does not do (undo it), and what the operator does first
/// (inspect and reset before retrying). Each file is checked on its own
/// wording, so a claim of automatic preservation cannot return in one of
/// them unnoticed.
#[test]
fn every_place_that_calls_the_writer_read_only_says_detection_not_undo() {
    let cases: [(&str, &[&str]); 8] = [
        (
            "workflows/coding-task.yaml",
            &[
                "the task parks, and choco does not undo that change",
                "The operator inspects the worktree and resets any change before retrying",
            ],
        ),
        (
            "workflows/experimental/review-panel.yaml",
            &[
                "the task parks, and choco does not undo that change",
                "The operator inspects the worktree and resets any change before retrying",
            ],
        ),
        (
            "docs/coding-task.md",
            &[
                "parks the task as stuck, and choco does not undo the change",
                "Inspect the worktree and reset any change before `choco task retry`",
            ],
        ),
        (
            "docs/workflows.md",
            &[
                "it does not undo it",
                "inspect the worktree, reset any change yourself, then run `choco task retry`",
            ],
        ),
        (
            ".claude/skills/run-choco-task/SKILL.md",
            &[
                "it parks the task and does not revert the change",
                "Inspect the worktree and reset any change before `choco task retry`",
            ],
        ),
        (
            ".claude/skills/run-choco-task/reference/stages.md",
            &[
                "parks the task as `stuck`; it does not undo the change",
                "Inspect the worktree and reset any change before `choco task retry`",
            ],
        ),
        (
            ".claude/skills/run-choco-task/reference/recover.md",
            &[
                "choco did not undo it",
                "inspect the worktree",
                "reset every change the writer made",
                "resumes its session against its original baseline",
                "starts a fresh session that takes whatever is in the worktree",
            ],
        ),
        (
            "docs/cli.md",
            &["`describe_pr`", "the daemon answers `409`"],
        ),
    ];
    for (file, clauses) in cases {
        let text = flat(&read(file));
        for clause in clauses {
            assert!(text.contains(clause), "{file} lacks: {clause}");
        }
        for claim in [
            "leaves the worktree unchanged",
            "restores the worktree",
            "automatically preserve",
            "automatically undo",
            "automatically reset",
            "a plain retry is safe",
        ] {
            assert!(!text.contains(claim), "{file} claims: {claim}");
        }
    }
    // The retry caveat sits wherever a plain retry is mentioned for the writer.
    for file in [
        "docs/coding-task.md",
        ".claude/skills/run-choco-task/SKILL.md",
        ".claude/skills/run-choco-task/reference/stages.md",
        ".claude/skills/run-choco-task/reference/recover.md",
        "docs/workflows.md",
    ] {
        assert!(
            flat(&read(file)).contains("a plain retry"),
            "{file} must say what a plain retry does"
        );
    }
}
