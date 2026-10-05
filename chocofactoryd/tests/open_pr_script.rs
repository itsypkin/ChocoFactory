//! Runs the real `workflows/scripts/open-pr.sh` (#101) in a linked worktree
//! of a temp repo whose `origin` is a bare temp repo, with a fake `gh` first
//! on `PATH` that records every call byte for byte.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_GH: &str = r#"#!/bin/sh
LOG="$GH_FAKE_DIR/log"
CFG="$GH_FAKE_DIR/cfg"
n=$(cat "$LOG/count" 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > "$LOG/count"
call="$LOG/call-$n"
mkdir -p "$call"
printf '%s %s' "${1:-}" "${2:-}" > "$call/sub"
printf '%s' "$#" > "$call/argc"
i=0
for a in "$@"; do
    i=$((i+1))
    printf '%s' "$a" > "$call/arg-$i"
done
prev=""
for a in "$@"; do
    if [ "$prev" = "--body-file" ]; then cp "$a" "$call/body"; fi
    prev=$a
done
if [ -e "$CFG/fail-${1:-}-${2:-}" ]; then
    # A create that fails after the PR exists must still fail the script, so
    # the read-back cannot be what makes the test pass.
    if [ "${1:-} ${2:-}" = "pr create" ]; then touch "$LOG/created"; fi
    echo "fake gh: configured to fail" >&2
    exit 1
fi
case "${1:-} ${2:-}" in
"pr list")
    case "$*" in
    *number,url*)
        if [ ! -e "$CFG/empty-readback" ] && { [ -e "$CFG/open-number" ] || [ -e "$LOG/created" ]; }; then
            printf '{"number":7,"url":"https://example.test/pull/7"}\n'
        fi
        ;;
    *)
        if [ -e "$CFG/open-number" ]; then cat "$CFG/open-number"; fi
        ;;
    esac
    ;;
"pr view")
    # Like the real command: `-t` prints the body exactly, `-q` (jq) adds a
    # trailing newline that GitHub never stored.
    case "$*" in
    *"-t"*) cat "$CFG/body" ;;
    *) cat "$CFG/body"; printf '\n' ;;
    esac
    ;;
"pr create") touch "$LOG/created" ;;
esac
exit 0
"#;

struct Fixture {
    root: PathBuf,
    wt: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.test")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.test")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("choco-open-pr-{}", uuid::Uuid::new_v4()));
        let origin = root.join("origin.git");
        let repo = root.join("repo");
        let wt = root.join("wt");
        fs::create_dir_all(&origin).unwrap();
        fs::create_dir_all(&repo).unwrap();
        git(&origin, &["init", "--bare", "-q"]);
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("README"), "hi\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "-q", "origin", "main"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/branch-name-xyz",
                wt.to_str().unwrap(),
            ],
        );
        for n in 1..=3 {
            fs::write(wt.join(format!("f{n}")), "x\n").unwrap();
            git(&wt, &["add", "."]);
            git(&wt, &["commit", "-q", "-m", &format!("commit {n}")]);
        }

        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, FAKE_GH).unwrap();
        make_executable(&gh);
        let gh_dir = root.join("gh");
        fs::create_dir_all(gh_dir.join("log")).unwrap();
        fs::create_dir_all(gh_dir.join("cfg")).unwrap();
        Fixture { root, wt }
    }

    fn git_dir(&self) -> PathBuf {
        let raw = git(&self.wt, &["rev-parse", "--git-dir"]);
        let raw = PathBuf::from(raw.trim());
        let abs = if raw.is_absolute() {
            raw
        } else {
            self.wt.join(raw)
        };
        abs.canonicalize().unwrap()
    }

    fn description_path(&self) -> PathBuf {
        self.git_dir().join("choco-pr-description.md")
    }

    fn write_description(&self, bytes: &[u8]) {
        fs::write(self.description_path(), bytes).unwrap();
    }

    fn cfg(&self, name: &str, contents: &str) {
        fs::write(self.root.join("gh/cfg").join(name), contents).unwrap();
    }

    fn run(&self, title: &str, verdict: &str, report: &str) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/scripts/open-pr.sh");
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        Command::new(script)
            .current_dir(&self.wt)
            .env("PATH", path)
            .env("GH_FAKE_DIR", self.root.join("gh"))
            .env("PR_TASK_TITLE", title)
            .env("PR_REVIEW_VERDICT", verdict)
            .env("PR_REVIEW_REPORT", report)
            .env("CHOCO_TASK_ID", "task-123")
            .env("CHOCO_WORKFLOW", "coding-task")
            .env(
                "CHOCO_ROLE_MODELS",
                "coder=claude-sonnet-5-5, reviewer=claude-opus-5-5",
            )
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<Call> {
        let log = self.root.join("gh/log");
        let count: usize = fs::read_to_string(log.join("count"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0);
        (1..=count)
            .map(|n| {
                let dir = log.join(format!("call-{n}"));
                let argc: usize = fs::read_to_string(dir.join("argc"))
                    .unwrap()
                    .parse()
                    .unwrap();
                Call {
                    sub: fs::read_to_string(dir.join("sub")).unwrap(),
                    args: (1..=argc)
                        .map(|i| fs::read(dir.join(format!("arg-{i}"))).unwrap())
                        .collect(),
                    body: fs::read(dir.join("body")).ok(),
                }
            })
            .collect()
    }

    fn calls_to(&self, sub: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.sub == sub).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Call {
    sub: String,
    args: Vec<Vec<u8>>,
    body: Option<Vec<u8>>,
}

impl Call {
    fn has_arg(&self, wanted: &[u8]) -> bool {
        self.args.iter().any(|a| a == wanted)
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

const BEGIN: &str = "<!-- choco:pr-description:begin -->";
const END: &str = "<!-- choco:pr-description:end -->";
const READ_BACK: &str = "{\"number\":7,\"url\":\"https://example.test/pull/7\"}\n";

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn hostile(dir: &Path) -> String {
    let d = dir.display();
    format!("q\"uote 's' $(touch {d}/a) `touch {d}/b` ; touch {d}/c \\ %s\nline two %s \\n done")
}

#[test]
fn first_lap_creates_a_named_described_pr() {
    let fx = Fixture::new();
    fx.write_description(b"## Problem\nIt was broken.\n");
    let out = fx.run("Name the PRs (#101)", "approved", "All good.\nSecond line.");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), READ_BACK);

    let creates = fx.calls_to("pr create");
    assert_eq!(creates.len(), 1);
    let create = &creates[0];
    assert!(create.has_arg(b"--title=Name the PRs (#101)"));
    assert!(create.has_arg(b"--body-file"));
    for arg in &create.args {
        let arg = String::from_utf8_lossy(arg);
        assert!(!arg.starts_with("--fill"), "{arg}");
        assert_ne!(arg, "task/branch-name-xyz");
    }
    let body = String::from_utf8(create.body.clone().unwrap()).unwrap();
    assert!(body.starts_with(&format!(
        "{BEGIN}\nCloses #101\n\n## Problem\nIt was broken.\n"
    )));
    assert!(body.contains("Verdict: **approved**."));
    assert!(body.contains(
        "<details>\n<summary>Internal reviewer's report</summary>\n\nAll good.\nSecond line.\n\n</details>"
    ));
    assert!(body.contains(
        "Opened by choco task `task-123` · workflow `coding-task` · coder=claude-sonnet-5-5, reviewer=claude-opus-5-5\n"
    ));
    assert!(body.ends_with(&format!("{END}\n")));
    assert!(fx.calls().iter().all(|c| c.sub != "pr comment"));
}

#[test]
fn hostile_text_is_never_parsed_by_a_shell() {
    let fx = Fixture::new();
    let dir = fx.root.join("marks");
    fs::create_dir_all(&dir).unwrap();
    // Short enough to survive the 100-byte title limit untouched; relative
    // marker paths land in the worktree if a shell ever ran them.
    let title = "-q\"uote 's' $(touch a) `touch b` ; touch c \\ %s\nx".to_string();
    let description = format!("DESC:{}\n", hostile(&dir));
    let report = format!("REPORT:{}", hostile(&dir));
    fx.write_description(description.as_bytes());
    let out = fx.run(&title, "approved", &report);
    assert!(out.status.success(), "{}", stderr(&out));

    let collapsed = title.replace('\n', " ");
    let create = &fx.calls_to("pr create")[0];
    assert!(create.has_arg(format!("--title={collapsed}").as_bytes()));
    let body = String::from_utf8(create.body.clone().unwrap()).unwrap();
    assert!(body.contains(&description), "description bytes changed");
    assert!(
        body.contains(&format!("{report}\n")),
        "report bytes changed"
    );
    for marker in ["a", "b", "c"] {
        assert!(!dir.join(marker).exists(), "{marker} was created");
    }
    for marker in ["a", "b", "c"] {
        assert!(
            !fx.wt.join(marker).exists(),
            "{marker} was created in the worktree"
        );
    }
}

#[test]
fn the_issue_line_follows_the_title() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run("Fix it (#101)", "approved", "r");
    assert!(out.status.success());
    let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
    assert!(body.contains("\nCloses #101\n"));
    assert!(!stderr(&out).contains("no issue number"));

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run("Part of #101 and #7 here", "approved", "r");
    assert!(out.status.success());
    let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
    assert!(body.contains("\nRefs #101\n"));
    assert!(!body.contains("Closes"));

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run("No number here", "approved", "r");
    assert!(out.status.success());
    let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
    assert!(body.contains("\nNo linked issue: the task's title names none.\n"));
    assert!(stderr(&out).contains("choco open-pr: note: no issue number in the task title"));
}

#[test]
fn a_long_multibyte_title_is_shortened_on_a_character_boundary() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let title = format!("{} (#101)", "é".repeat(75)); // 150 + 7 bytes
    assert!(title.len() > 150);
    let out = fx.run(&title, "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    let create = &fx.calls_to("pr create")[0];
    let arg = create
        .args
        .iter()
        .find_map(|a| a.strip_prefix(b"--title="))
        .unwrap();
    let t = std::str::from_utf8(arg).expect("valid UTF-8");
    assert!(t.len() <= 100, "{} bytes", t.len());
    assert!(t.ends_with("… (#101)"), "{t}");
}

#[test]
fn a_missing_or_blank_description_is_noted_not_fatal() {
    for blank in [None, Some("  \n\t\n")] {
        let fx = Fixture::new();
        if let Some(text) = blank {
            fx.write_description(text.as_bytes());
        }
        let out = fx.run("T (#1)", "approved", "the report");
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
        assert!(body.contains("**The coder wrote no description for this PR.**"));
        assert!(body.contains("the report"));
        assert!(body.contains("Opened by choco task `task-123`"));
        assert!(stderr(&out).contains("choco open-pr: note: no PR description at "));
    }
}

#[test]
fn an_empty_report_says_none_was_captured() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run("T (#1)", "", "");
    assert!(out.status.success());
    let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
    assert!(body.contains("No internal review report was captured for this task."));
    assert!(!body.contains("<details>"));
}

#[test]
fn a_later_lap_replaces_only_the_generated_block() {
    let fx = Fixture::new();
    fx.write_description(b"new description\n");
    fx.cfg("open-number", "7\n");
    let old =
        format!("human intro\r\nmore\r\n{BEGIN}\nold block\n{END}\r\nhuman outro\r\nlast\r\n");
    fx.cfg("body", &old);
    let out = fx.run("T (#1)", "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), READ_BACK);

    assert!(fx.calls_to("pr create").is_empty());
    let edits = fx.calls_to("pr edit");
    assert_eq!(edits.len(), 1);
    assert!(edits[0].has_arg(b"7"));
    assert!(edits[0].has_arg(b"--body-file"));
    assert!(!edits[0].args.iter().any(|a| a.starts_with(b"--title")));
    let body = String::from_utf8(edits[0].body.clone().unwrap()).unwrap();
    assert!(body.starts_with("human intro\r\nmore\r\n"), "{body:?}");
    assert!(body.ends_with("\nhuman outro\r\nlast\r\n"), "{body:?}");
    assert!(body.contains("new description"));
    assert!(!body.contains("old block"));
}

#[test]
fn a_body_without_exactly_one_block_is_left_alone() {
    let two_begins = format!("a\n{BEGIN}\nx\n{BEGIN}\ny\n{END}\n");
    let cases = [
        "hand written\n".to_string(),
        two_begins,
        format!("{END}\nx\n{BEGIN}\n"),
    ];
    for body in cases {
        let fx = Fixture::new();
        fx.write_description(b"d\n");
        fx.cfg("open-number", "7\n");
        fx.cfg("body", &body);
        let out = fx.run("T (#1)", "approved", "r");
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert!(fx.calls_to("pr edit").is_empty());
        assert!(fx.calls_to("pr create").is_empty());
        assert!(
            stderr(&out).contains("doesn't have exactly one choco block"),
            "{}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), READ_BACK);
    }
}

#[test]
fn marker_lines_in_agent_text_are_stripped() {
    let fx = Fixture::new();
    fx.write_description(format!("one\n{BEGIN}\ntwo\n{END}  \nthree\n").as_bytes());
    let out = fx.run(
        "T (#1)",
        "approved",
        &format!("r1\n{BEGIN}\r\nr2\n{END}\t\nr3"),
    );
    assert!(out.status.success());
    let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
    assert_eq!(body.matches(BEGIN).count(), 1);
    assert_eq!(body.matches(END).count(), 1);
    assert!(body.contains("one\ntwo\nthree\n"));
    assert!(body.contains("r1\nr2\nr3\n"));
}

#[test]
fn oversized_text_is_capped_with_a_truncation_line() {
    let fx = Fixture::new();
    let line = "é".repeat(49) + "\n"; // 99 bytes
    let big = line.repeat(1100); // ~108 KB
    assert!(big.len() > 100_000);
    fx.write_description(big.as_bytes());
    let out = fx.run("T (#1)", "approved", &big);
    assert!(out.status.success(), "{}", stderr(&out));
    let bytes = fx.calls_to("pr create")[0].body.clone().unwrap();
    assert!(bytes.len() < 60_000, "{} bytes", bytes.len());
    let body = String::from_utf8(bytes).expect("valid UTF-8");
    assert_eq!(body.matches("[truncated: ").count(), 2, "{body}");
}

#[test]
fn failures_exit_nonzero() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run("  \n\t ", "approved", "r");
    assert!(!out.status.success());
    assert!(fx.calls_to("pr create").is_empty() && fx.calls_to("pr edit").is_empty());
    let remote = git(&fx.root.join("origin.git"), &["branch", "--list", "task/*"]);
    assert!(
        remote.trim().is_empty(),
        "branch pushed despite empty title: {remote}"
    );

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("fail-pr-create", "");
    assert!(!fx.run("T (#1)", "approved", "r").status.success());

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("open-number", "7\n");
    fx.cfg("body", &format!("{BEGIN}\nold\n{END}\n"));
    fx.cfg("fail-pr-view", "");
    let out = fx.run("T (#1)", "approved", "r");
    assert!(!out.status.success());
    assert!(fx.calls_to("pr edit").is_empty() && fx.calls_to("pr create").is_empty());

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("fail-pr-list", "");
    let out = fx.run("T (#1)", "approved", "r");
    assert!(!out.status.success());
    assert!(fx.calls_to("pr edit").is_empty() && fx.calls_to("pr create").is_empty());

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("open-number", "7\n");
    fx.cfg("body", &format!("{BEGIN}\nold\n{END}\n"));
    fx.cfg("fail-pr-edit", "");
    assert!(!fx.run("T (#1)", "approved", "r").status.success());

    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("empty-readback", "");
    let out = fx.run("T (#1)", "approved", "r");
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "");
}

#[test]
fn the_description_is_read_from_the_worktrees_private_git_dir() {
    let fx = Fixture::new();
    let git_dir = fx.git_dir();
    assert!(
        git_dir.to_string_lossy().contains("worktrees"),
        "not a linked worktree: {git_dir:?}"
    );
    assert!(!git_dir.starts_with(fx.wt.canonicalize().unwrap()));
    fx.write_description(b"FROM-PRIVATE-GIT-DIR\n");
    // A decoy inside the work tree must not be read.
    fs::write(fx.wt.join("choco-pr-description.md"), "DECOY\n").unwrap();
    let out = fx.run("T (#1)", "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    let body = String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap();
    assert!(body.contains("FROM-PRIVATE-GIT-DIR"));
    assert!(!body.contains("DECOY"));

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows");
    let script = fs::read_to_string(root.join("scripts/open-pr.sh")).unwrap();
    let coder = fs::read_to_string(root.join("prompts/coder-system.md")).unwrap();
    let reviewer = fs::read_to_string(root.join("prompts/reviewer-turn.md")).unwrap();
    for (name, text) in [
        ("open-pr.sh", &script),
        ("coder-system.md", &coder),
        ("reviewer-turn.md", &reviewer),
    ] {
        assert!(!text.contains("--path-format"), "{name}");
    }
    let path_cmd = r#""$(cd "$(git rev-parse --git-dir)" && pwd)/choco-pr-description.md""#;
    for (name, text) in [
        ("open-pr.sh", &script),
        ("coder-system.md", &coder),
        ("reviewer-turn.md", &reviewer),
    ] {
        assert!(text.contains(path_cmd), "{name} lacks the path command");
    }
}

// ---- #131: agent text can never close an issue ----

const KEYWORDS: &str = "closes|closed|close|fixes|fixed|fix|resolves|resolved|resolve";

/// Keyword, optional colon, optional whitespace (one newline allowed), then
/// any reference form. Built here from the documented syntax, independent of
/// the script.
fn closing_regex() -> regex::Regex {
    let refs = r"(?:https?://(?:www\.)?github\.com/[\w.-]+/[\w.-]+/issues/\d+|(?:www\.)?github\.com/[\w.-]+/[\w.-]+/issues/\d+|[\w.-]+/[\w.-]+#\d+|#\d+|GH-\d+|<URL>|\[[^\]\n]*\]\(URL\))";
    // `URL` stands for the issue-URL alternation (scheme and www optional).
    let url = r"(?:https?://)?(?:www\.)?github\.com/[\w.-]+/[\w.-]+/issues/\d+";
    let refs = refs.replace("URL", url);
    regex::Regex::new(&format!(
        r"(?i)(?:^|[^A-Za-z0-9_])(?:{KEYWORDS})\b:?[ \t]*\r?\n?[ \t]*{refs}"
    ))
    .unwrap()
}

const AGENT_LINES: &str = "Its message has no Closes/Fixes/Resolves #84.\nfixes: #12\nCloses owner/repo#3\ncloses https://github.com/o/r/issues/5\nFIXED #6\nresolved:#7\ncloses http://www.github.com/o/r/issues/55/\nfixes https://github.com/o/r/issues/6#issuecomment-1 tail\nresolves github.com/o/r/issues/7?x=1\nfixed:\n#78\nfixes #1 and closes #2\nFixes GH-13\nfixes gh-17\nFixes <https://github.com/o/r/issues/21>\nFixes [#12](https://github.com/o/r/issues/12)\nFixes [issue 18](https://github.com/o/r/issues/18)";

fn body_for(title: &str, desc: Option<&[u8]>, report: &str) -> String {
    let fx = Fixture::new();
    if let Some(d) = desc {
        fx.write_description(d);
    }
    let out = fx.run(title, "approved", report);
    assert!(out.status.success(), "{}", stderr(&out));
    String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap()
}

fn assert_rewritten(body: &str) {
    for want in [
        "Resolves issue 84",
        "fixes: issue 12",
        "Closes owner/repo issue 3",
        "closes o/r issue 5",
        "FIXED issue 6",
        "resolved:issue 7",
        "closes o/r issue 55\n",
        "fixes o/r issue 6 tail",
        "resolves o/r issue 7\n",
        "fixed:\nissue 78",
        "fixes issue 1 and closes issue 2",
        "Fixes issue 13\n",
        "fixes issue 17\n",
        "Fixes o/r issue 21\n",
        "Fixes o/r issue 12\n",
        "Fixes o/r issue 18",
    ] {
        assert!(body.contains(want), "missing {want:?} in {body}");
    }
}

#[test]
fn the_84_regression_report_cannot_close_issues() {
    let body = body_for("T (#131)", Some(b"d\n"), AGENT_LINES);
    assert_rewritten(&body);
    assert_eq!(body.matches("Closes #131\n").count(), 1);
}

#[test]
fn only_the_scripts_own_issue_line_matches_a_closing_pattern() {
    let re = closing_regex();
    let body = body_for("T (#131)", Some(AGENT_LINES.as_bytes()), AGENT_LINES);
    let found: Vec<_> = re.find_iter(&body).map(|m| m.as_str().trim()).collect();
    assert_eq!(found, vec!["Closes #131"], "{body}");
    let body = body_for("T (#84 part 4)", Some(AGENT_LINES.as_bytes()), AGENT_LINES);
    assert!(body.contains("\nRefs #84\n"));
    assert_eq!(re.find_iter(&body).count(), 0, "{body}");
}

#[test]
fn the_description_is_neutralised_the_same_way() {
    let body = body_for("T (#131)", Some(AGENT_LINES.as_bytes()), "r");
    assert_rewritten(&body);
}

#[test]
fn bare_references_are_untouched() {
    let text = "see #98\nowner/repo#3 is related\nhttps://github.com/o/r/issues/5\n";
    let body = body_for("T (#1)", Some(text.as_bytes()), text.trim_end());
    assert_eq!(body.matches(text).count(), 2, "{body}");
}

#[test]
fn lookalike_words_are_not_keywords() {
    let text = "unfixed #6\nprefixes #7\nenclosed #8\nclosest #9\nfixture #10\n";
    let body = body_for("T (#1)", Some(text.as_bytes()), text.trim_end());
    assert_eq!(body.matches(text).count(), 2, "{body}");
}

#[test]
fn a_reference_on_the_next_line_is_rewritten_but_not_two_lines_down() {
    let body = body_for("T (#1)", Some(b"This fixes\n#77 as well\n"), "r");
    assert!(body.contains("This fixes\nissue 77 as well\n"), "{body}");
    let body = body_for("T (#1)", Some(b"This fixes\nsomething\n#77 as well\n"), "r");
    assert!(
        body.contains("This fixes\nsomething\n#77 as well\n"),
        "{body}"
    );
}

#[test]
fn only_the_first_reference_after_a_keyword_is_rewritten() {
    let body = body_for("T (#1)", Some(b"Fixes #1, #2\n"), "r");
    assert!(body.contains("Fixes issue 1, #2\n"), "{body}");
}

#[test]
fn code_is_neutralised_too() {
    let body = body_for(
        "T (#1)",
        Some(b"```\nFixes #5\n```\nand `Fixes #5` inline\n"),
        "r",
    );
    assert!(body.contains("```\nFixes issue 5\n```\n"), "{body}");
    assert!(body.contains("`Fixes issue 5` inline"), "{body}");
}

#[test]
fn text_without_keywords_keeps_every_byte() {
    let text = "caf\u{e9} \u{2014} \u{65e5}\u{672c}\r\nline with spaces   \r\n\ttabbed\r\n";
    let body = body_for("T (#1)", Some(text.as_bytes()), "r");
    assert!(body.contains(text), "{body:?}");
}

#[test]
fn the_caps_still_hold_for_keyword_floods() {
    let big = "Fixes #1 ".repeat(12_000);
    assert!(big.len() > 100_000);
    let body = body_for("T (#131)", Some(big.as_bytes()), &big);
    assert!(body.len() < 60_000, "{} bytes", body.len());
    assert_eq!(body.matches("[truncated: ").count(), 2);
    let found: Vec<_> = closing_regex()
        .find_iter(&body)
        .map(|m| m.as_str().trim().to_string())
        .collect();
    assert_eq!(found, vec!["Closes #131"]);
}

#[test]
fn crlf_and_indented_next_line_references_are_rewritten() {
    let body = body_for("T (#1)", Some(b"This fixes\r\n  #77 too\r\n"), "r");
    assert!(
        body.contains("This fixes\r\n  issue 77 too\r\n"),
        "{body:?}"
    );
}

// Checked against real GitHub on 2026-10-05: none of these closes an issue,
// so the filter must leave them alone.
#[test]
fn verified_non_linking_forms_pass_through_byte_for_byte() {
    let text = "**Fixes** #1\n_Fixes_ #2\nFixes **#3**\n*Fixes* #15\nFixes _#16_\nFixes&nbsp;#19\nFixes\u{a0}#7\n";
    let body = body_for("T (#1)", Some(text.as_bytes()), text.trim_end());
    assert_eq!(body.matches(text).count(), 2, "{body}");
}

#[test]
fn new_forms_on_the_next_line_are_rewritten() {
    for (input, want) in [
        ("This fixes\nGH-77 too\n", "This fixes\nissue 77 too\n"),
        (
            "This fixes\n<https://github.com/o/r/issues/77>\n",
            "This fixes\no/r issue 77\n",
        ),
        (
            "This fixes\n[x](https://github.com/o/r/issues/77)\n",
            "This fixes\no/r issue 77\n",
        ),
    ] {
        let body = body_for("T (#1)", Some(input.as_bytes()), "r");
        assert!(body.contains(want), "{body}");
    }
}

#[test]
fn near_misses_are_not_rewritten() {
    let text = "Fixes GH-\nFixes [x](https://github.com/o/r/pull/4)\nFixes <https://example.com/issues/4>\nFixes [x](https://github.com/o/r/issues/4\nFixes <https://github.com/o/r/issues/4\n";
    let body = body_for("T (#1)", Some(text.as_bytes()), text.trim_end());
    assert_eq!(body.matches(text).count(), 2, "{body}");
}

#[test]
fn new_forms_keep_the_tail_and_only_the_first_reference() {
    let text = "Fixes <https://github.com/o/r/issues/5#c-1> and GH-6\nFixes [a](https://github.com/o/r/issues/7?x=1) then [b](https://github.com/o/r/issues/8)\n";
    let body = body_for("T (#1)", Some(text.as_bytes()), "r");
    assert!(body.contains("Fixes o/r issue 5 and GH-6\n"), "{body}");
    assert!(
        body.contains("Fixes o/r issue 7 then [b](https://github.com/o/r/issues/8)\n"),
        "{body}"
    );
}

/// Runs the script with an awk that fails whenever the filter's program runs.
fn run_with_failing_filter(desc: Option<&[u8]>, report: &str) {
    let fx = Fixture::new();
    if let Some(d) = desc {
        fx.write_description(d);
    }
    let fake = fx.root.join("bin/awk");
    let real = Command::new("sh")
        .args(["-c", "command -v awk"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap().trim().to_string();
    assert!(!real.is_empty(), "no awk on this host");
    fs::write(
        &fake,
        format!("#!/bin/sh\ncase \"$*\" in *refat*) exit 2;; esac\nexec {real} \"$@\"\n"),
    )
    .unwrap();
    make_executable(&fake);
    let out = fx.run("T (#131)", "approved", report);
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(fx.calls_to("pr create").is_empty());
}

#[test]
fn a_failing_filter_aborts_on_the_description_alone() {
    run_with_failing_filter(Some(b"d\n"), "");
}

#[test]
fn a_failing_filter_aborts_on_the_report_alone() {
    run_with_failing_filter(None, "r");
}

#[test]
fn text_cut_before_the_filter_is_never_dropped_silently() {
    // A URL fragment swallows the long token, so the rewritten text is tiny.
    let desc = format!(
        "closes https://github.com/o/r/issues/5#{}\nIMPORTANT TAIL\n",
        "a".repeat(70_000)
    );
    let body = body_for("T (#131)", Some(desc.as_bytes()), "r");
    assert!(body.contains("IMPORTANT TAIL") || body.contains("[truncated: "));
    // A plain 200 KB description reports its real size.
    let big = "word ".repeat(40_000);
    let body = body_for("T (#131)", Some(big.as_bytes()), "r");
    assert!(
        body.contains(&format!("of {} bytes shown]", big.len() + 1)),
        "wrong or missing total"
    );
}
