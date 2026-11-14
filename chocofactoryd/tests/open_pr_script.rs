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
    *headRefOid*)
        if [ -e "$CFG/pr-head" ]; then cat "$CFG/pr-head"; fi
        ;;
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
"pr edit")
    # Additive control for one test: replace the saved copy by a directory
    # after the edit body is recorded, as a disk problem would.
    if [ -e "$CFG/copy-to-dir" ]; then
        p=$(cat "$CFG/copy-to-dir")
        rm -rf "$p"
        mkdir "$p"
    fi
    ;;
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
        self.run_closes(title, Some(""), verdict, report)
    }

    /// Like `run`, with `CHOCO_TASK_CLOSES` set to `closes`, or removed from
    /// the environment for `None`. The variable is always set or removed
    /// explicitly, so one in the outer environment never leaks in.
    fn run_closes(&self, title: &str, closes: Option<&str>, verdict: &str, report: &str) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workflows/scripts/open-pr.sh");
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(script);
        match closes {
            Some(value) => cmd.env("CHOCO_TASK_CLOSES", value),
            None => cmd.env_remove("CHOCO_TASK_CLOSES"),
        };
        cmd.current_dir(&self.wt)
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
    let out = fx.run_closes(
        "Name the PRs (#101)",
        Some("101"),
        "approved",
        "All good.\nSecond line.",
    );
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

fn pr_body(fx: &Fixture) -> String {
    String::from_utf8(fx.calls_to("pr create")[0].body.clone().unwrap()).unwrap()
}

fn body_with_closes(title: &str, closes: Option<&str>) -> (String, String) {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run_closes(title, closes, "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    (pr_body(&fx), stderr(&out))
}

#[test]
fn the_closes_list_makes_the_closing_lines_at_the_top_of_the_block() {
    let (body, _) = body_with_closes("Do things", Some("152 126 130"));
    assert!(
        body.starts_with(&format!(
            "{BEGIN}\nCloses #152\nCloses #126\nCloses #130\n\n"
        )),
        "{body}"
    );
    assert_eq!(body.matches("Closes").count(), 3);
}

#[test]
fn a_repeated_number_gives_one_closing_line() {
    let (body, _) = body_with_closes("Do things", Some("5 5"));
    assert_eq!(body.matches("Closes #5\n").count(), 1, "{body}");
}

#[test]
fn a_number_in_the_title_only_links_when_nothing_is_closed() {
    for closes in [Some(""), None] {
        let (body, _) = body_with_closes("Fix X (#42)", closes);
        assert!(body.contains("\nRefs #42\n"), "{body}");
        assert!(!body.contains("Closes"), "{body}");
    }
}

#[test]
fn a_closed_number_in_the_title_is_not_linked_as_well() {
    let (body, _) = body_with_closes("Fix X (#42)", Some("42"));
    assert!(body.contains("\nCloses #42\n"), "{body}");
    assert!(!body.contains("Refs #42"), "{body}");
}

#[test]
fn every_distinct_title_number_gets_one_refs_line_in_title_order() {
    let (body, _) = body_with_closes("Part of #3 and #4, again #3", Some(""));
    assert_eq!(body.matches("Refs #").count(), 2, "{body}");
    assert!(body.contains("\nRefs #3\nRefs #4\n"), "{body}");
    // A leading zero is not an issue number.
    let (body, _) = body_with_closes("Odd #012", Some(""));
    assert!(!body.contains("Refs"), "{body}");
}

#[test]
fn closes_lines_come_before_refs_lines() {
    let (body, _) = body_with_closes("Part of #3 (#4)", Some("9 4"));
    assert!(
        body.starts_with(&format!("{BEGIN}\nCloses #9\nCloses #4\nRefs #3\n\n")),
        "{body}"
    );
}

#[test]
fn agent_text_cannot_close_anything_beyond_the_closes_list() {
    let fx = Fixture::new();
    fx.write_description(b"Fixes #126\n");
    let out = fx.run_closes("Do things", Some("152"), "approved", "Resolves #84");
    assert!(out.status.success(), "{}", stderr(&out));
    let body = pr_body(&fx);
    assert!(body.contains("Fixes issue 126"), "{body}");
    assert!(body.contains("Resolves issue 84"), "{body}");
    assert!(!body.contains("Fixes #126") && !body.contains("Resolves #84"));
    let found: Vec<_> = closing_regex()
        .find_iter(&body)
        .map(|m| m.as_str().trim().to_string())
        .collect();
    assert_eq!(found, vec!["Closes #152"], "{body}");
}

#[test]
fn no_closes_and_no_title_number_says_so() {
    let (body, err) = body_with_closes("No number here", Some(""));
    assert!(
        body.contains("\nNo linked issue: the task closes none and its title names none.\n"),
        "{body}"
    );
    assert!(!body.contains("Closes"), "{body}");
    assert!(
        err.contains("choco open-pr: note: no issue to close or link"),
        "{err}"
    );
}

#[test]
fn a_malformed_closes_value_fails_before_anything_is_pushed() {
    for bad in ["12 abc", "0", "#12", "012", "*"] {
        let fx = Fixture::new();
        fx.write_description(b"d\n");
        let out = fx.run_closes("Fix X", Some(bad), "approved", "r");
        assert!(!out.status.success(), "{bad}");
        let tok = bad.split(' ').next_back().unwrap();
        let err = stderr(&out);
        assert!(
            err.contains(&format!("CHOCO_TASK_CLOSES has '{tok}'")),
            "{bad}: {err}"
        );
        assert!(stdout(&out).is_empty());
        assert!(fx.calls().is_empty(), "{bad}: gh was called");
        let remote = git(&fx.root.join("origin.git"), &["branch", "--list", "task/*"]);
        assert!(
            remote.trim().is_empty(),
            "{bad}: branch was pushed: {remote}"
        );
    }
}

#[test]
fn a_refresh_turns_an_old_closing_line_into_a_link_and_keeps_human_text() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("open-number", "7\n");
    fx.cfg(
        "body",
        &format!("human intro\n{BEGIN}\nCloses #42\n\nold\n{END}\nhuman outro\n"),
    );
    let out = fx.run_closes("Fix X (#42)", Some(""), "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    let edits = fx.calls_to("pr edit");
    assert_eq!(edits.len(), 1);
    let body = String::from_utf8(edits[0].body.clone().unwrap()).unwrap();
    assert!(
        body.contains("\nRefs #42\n") && !body.contains("Closes"),
        "{body}"
    );
    assert!(body.starts_with("human intro\n"), "{body}");
    assert!(body.ends_with("\nhuman outro\n"), "{body}");
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

fn created_title(title: &str) -> String {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run(title, "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    let create = &fx.calls_to("pr create")[0];
    let arg = create
        .args
        .iter()
        .find_map(|a| a.strip_prefix(b"--title="))
        .unwrap();
    std::str::from_utf8(arg).expect("valid UTF-8").to_string()
}

#[test]
fn a_long_title_cut_mid_word_ends_at_the_previous_whole_word() {
    // Budget is 100 - 3 - 7 = 90 bytes; the 90th byte falls inside "bbbbbbbbbb".
    let title = format!("{} {} tail (#101)", "a".repeat(80), "b".repeat(30));
    let t = created_title(&title);
    assert_eq!(t, format!("{}… (#101)", "a".repeat(80)));
    assert!(t.len() <= 100);
}

#[test]
fn a_long_title_cut_exactly_on_a_space_keeps_the_last_word() {
    // 89 bytes of words, then a space as the 90th byte... the cut keeps the
    // whole word before it and REST starts at the space.
    let word = "a".repeat(84);
    let title = format!("{word} bbbbb {} (#101)", "c".repeat(30));
    let t = created_title(&title);
    assert_eq!(t, format!("{word} bbbbb… (#101)"));
}

#[test]
fn a_long_title_of_one_unbroken_word_is_cut_at_the_byte_budget() {
    let t = created_title(&format!("{} (#101)", "x".repeat(150)));
    assert_eq!(t, format!("{}… (#101)", "x".repeat(90)));
    assert_eq!(t.len(), 100);
}

#[test]
fn a_long_title_with_multibyte_words_is_cut_at_a_whole_word() {
    let title = format!("{} {} (#101)", "é".repeat(40), "é".repeat(40)); // 80 + 1 + 80 bytes
    let t = created_title(&title);
    assert!(t.len() <= 100, "{} bytes", t.len());
    assert_eq!(t, format!("{}… (#101)", "é".repeat(40)));
}

#[test]
fn a_title_of_exactly_100_bytes_is_unchanged_and_a_long_one_without_a_number_gets_no_suffix() {
    let exact = format!("{} b", "a".repeat(98));
    assert_eq!(exact.len(), 100);
    assert_eq!(created_title(&exact), exact);

    let t = created_title(&format!("{} {}", "a".repeat(60), "b".repeat(60)));
    assert_eq!(t, format!("{}…", "a".repeat(60)));
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
        assert!(!fx.git_dir().join("choco-open-pr-published.md").exists());
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
    let refs = r#"(?:https?://(?:www\.)?github\.com/[\w.-]+/[\w.-]+/issues/\d+|(?:www\.)?github\.com/[\w.-]+/[\w.-]+/issues/\d+|[\w.-]+/[\w.-]+#\d+|#\d+|GH-\d+|<URL>|\[[^\]\n]*\]\([ \t]*<?URL>?(?:[ \t]+(?:"[^"\n]*"|\x27[^\x27\n]*\x27|\([^()\n]*\)))?[ \t]*\))"#;
    // `URL` stands for the issue-URL alternation (scheme and www optional).
    let url = r"(?:https?://)?(?:www\.)?github\.com/[\w.-]+/[\w.-]+/issues/\d+";
    let refs = refs.replace("URL", url);
    regex::Regex::new(&format!(
        r"(?i)(?:^|[^A-Za-z0-9_])(?:{KEYWORDS})\b:?[ \t]*\r?\n?[ \t]*{refs}"
    ))
    .unwrap()
}

const AGENT_LINES: &str = "Its message has no Closes/Fixes/Resolves #84.\nfixes: #12\nCloses owner/repo#3\ncloses https://github.com/o/r/issues/5\nFIXED #6\nresolved:#7\ncloses http://www.github.com/o/r/issues/55/\nfixes https://github.com/o/r/issues/6#issuecomment-1 tail\nresolves github.com/o/r/issues/7?x=1\nfixed:\n#78\nfixes #1 and closes #2\nFixes GH-13\nfixes gh-17\nFixes <https://github.com/o/r/issues/21>\nFixes [#12](https://github.com/o/r/issues/12)\nFixes [issue 18](https://github.com/o/r/issues/18)\nFixes [x](https://github.com/o/r/issues/4 \"title\")\nFixes [x](<https://github.com/o/r/issues/4>)\nFixes [x]( https://github.com/o/r/issues/4 )\nFixes [x](https://github.com/o/r/issues/4 'title')\nFixes [x](https://github.com/o/r/issues/4 (title))\nFixes gh-1/repo#3";

fn body_for(title: &str, desc: Option<&[u8]>, report: &str) -> String {
    body_for_closes(title, "", desc, report)
}

fn body_for_closes(title: &str, closes: &str, desc: Option<&[u8]>, report: &str) -> String {
    let fx = Fixture::new();
    if let Some(d) = desc {
        fx.write_description(d);
    }
    let out = fx.run_closes(title, Some(closes), "approved", report);
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
        "Fixes o/r issue 4\nFixes o/r issue 4\nFixes o/r issue 4\nFixes o/r issue 4\nFixes o/r issue 4\nFixes issue 1/repo#3",
    ] {
        assert!(body.contains(want), "missing {want:?} in {body}");
    }
}

#[test]
fn the_84_regression_report_cannot_close_issues() {
    let body = body_for_closes("T (#131)", "131", Some(b"d\n"), AGENT_LINES);
    assert_rewritten(&body);
    assert_eq!(body.matches("Closes #131\n").count(), 1);
}

#[test]
fn only_the_scripts_own_issue_line_matches_a_closing_pattern() {
    let re = closing_regex();
    let body = body_for_closes("T (#131)", "131", Some(AGENT_LINES.as_bytes()), AGENT_LINES);
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
    let body = body_for_closes("T (#131)", "131", Some(big.as_bytes()), &big);
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
    let text = "Fixes GH-\nFixes [x](https://github.com/o/r/pull/4)\nFixes <https://example.com/issues/4>\nFixes [x](https://github.com/o/r/issues/4\nFixes <https://github.com/o/r/issues/4\nFixes [x](<https://github.com/o/r/issues/4)\n";
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

const TASK_REF: &str = "refs/heads/task/branch-name-xyz";

fn origin_tip(fx: &Fixture) -> String {
    git(&fx.root.join("origin.git"), &["rev-parse", TASK_REF])
        .trim()
        .to_string()
}

fn first_line(out: &Output) -> String {
    stderr(out).lines().next().unwrap_or("").to_string()
}

/// Pushes to the task branch from a second clone of origin.
fn push_from_other_clone(fx: &Fixture) -> String {
    let other = fx.root.join("other");
    git(
        &fx.root,
        &[
            "clone",
            "-q",
            fx.root.join("origin.git").to_str().unwrap(),
            "other",
        ],
    );
    git(&other, &["checkout", "-q", "task/branch-name-xyz"]);
    fs::write(other.join("theirs"), "t\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "theirs"]);
    git(&other, &["push", "-q", "origin", "HEAD"]);
    origin_tip(fx)
}

fn amend_local(fx: &Fixture) {
    fs::write(fx.wt.join("f1"), "changed\n").unwrap();
    git(&fx.wt, &["add", "."]);
    git(&fx.wt, &["commit", "-q", "--amend", "-m", "amended"]);
}

fn assert_failed_cleanly(fx: &Fixture, out: &Output) {
    assert!(!out.status.success());
    assert!(stdout(out).is_empty(), "{}", stdout(out));
    assert!(fx.calls_to("pr create").is_empty() && fx.calls_to("pr edit").is_empty());
}

fn pr_exists(fx: &Fixture) {
    fx.cfg("open-number", "7\n");
    fx.cfg("body", &format!("{BEGIN}\nold\n{END}\n"));
}

#[test]
fn first_push_is_plain_and_looks_up_open_pr() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run("T (#1)", "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(origin_tip(&fx), git(&fx.wt, &["rev-parse", "HEAD"]).trim());
    let lists = fx.calls_to("pr list");
    assert!(lists.iter().any(|c| c.has_arg(b"--state")
        && c.has_arg(b"open")
        && c.has_arg(b"--json")
        && c.has_arg(b"number,headRefOid")));
}

#[test]
fn rebased_branch_with_open_pr_is_pushed_with_lease() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    fx.cfg("pr-head", &format!("{}\n", origin_tip(&fx)));
    pr_exists(&fx);
    amend_local(&fx);
    let out = fx.run("T (#1)", "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(origin_tip(&fx), git(&fx.wt, &["rev-parse", "HEAD"]).trim());
    let err = stderr(&out);
    assert!(!err.contains("moved since the PR's head") && !err.contains("rejected"));
}

#[test]
fn lease_refuses_when_someone_else_moved_the_branch() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    let old = origin_tip(&fx);
    fx.cfg("pr-head", &format!("{old}\n"));
    pr_exists(&fx);
    let theirs = push_from_other_clone(&fx);
    amend_local(&fx);
    let calls_before = fx.calls_to("pr edit").len();
    let out = fx.run("T (#1)", "approved", "r");
    assert!(!out.status.success());
    assert!(stdout(&out).is_empty());
    assert_eq!(fx.calls_to("pr edit").len(), calls_before);
    assert_eq!(origin_tip(&fx), theirs);
    assert_eq!(
        first_line(&out),
        format!(
            "choco open-pr: the branch on GitHub moved since the PR's head {old}: someone else pushed; look before resuming"
        )
    );
    assert!(stderr(&out).contains("(stale info)"));
}

#[test]
fn plain_non_fast_forward_without_pr_is_explained() {
    for fetch in [false, true] {
        let fx = Fixture::new();
        fx.write_description(b"d\n");
        assert!(fx.run("T (#1)", "approved", "r").status.success());
        let created = fx.calls_to("pr create").len();
        let theirs = push_from_other_clone(&fx);
        amend_local(&fx);
        if fetch {
            git(&fx.wt, &["fetch", "-q", "origin"]);
        }
        let out = fx.run("T (#1)", "approved", "r");
        assert!(!out.status.success());
        assert!(stdout(&out).is_empty());
        assert_eq!(fx.calls_to("pr create").len(), created);
        assert_eq!(origin_tip(&fx), theirs);
        assert!(
            first_line(&out).starts_with("choco open-pr: the push was rejected (non-fast-forward)"),
            "fetch={fetch}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn lookup_failure_pushes_nothing() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("fail-pr-list", "");
    let out = fx.run("T (#1)", "approved", "r");
    assert_failed_cleanly(&fx, &out);
    let remote = git(&fx.root.join("origin.git"), &["branch", "--list", "task/*"]);
    assert!(remote.trim().is_empty(), "{remote}");
    assert_eq!(
        first_line(&out),
        "choco open-pr: could not look up the open PR for task/branch-name-xyz; nothing was pushed"
    );
    assert!(stderr(&out).contains("fake gh: configured to fail"));
}

#[test]
fn malformed_head_oid_pushes_nothing() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("pr-head", "7\n");
    let out = fx.run("T (#1)", "approved", "r");
    assert_failed_cleanly(&fx, &out);
    let remote = git(&fx.root.join("origin.git"), &["branch", "--list", "task/*"]);
    assert!(remote.trim().is_empty(), "{remote}");
    assert!(first_line(&out).starts_with("choco open-pr: gh returned an unexpected head commit"));
}

#[test]
fn non_hex_head_oid_of_right_length_pushes_nothing() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    fx.cfg("pr-head", &format!("{}\n", "g".repeat(40)));
    let out = fx.run("T (#1)", "approved", "r");
    assert_failed_cleanly(&fx, &out);
    let remote = git(&fx.root.join("origin.git"), &["branch", "--list", "task/*"]);
    assert!(remote.trim().is_empty(), "{remote}");
    assert!(first_line(&out).starts_with("choco open-pr: gh returned an unexpected head commit"));
}

#[test]
fn other_push_failure_gets_generic_message() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let missing = fx.root.join("missing.git");
    git(
        &fx.wt,
        &["remote", "set-url", "origin", missing.to_str().unwrap()],
    );
    let out = fx.run("T (#1)", "approved", "r");
    assert_failed_cleanly(&fx, &out);
    assert_eq!(
        first_line(&out),
        "choco open-pr: the push failed; git's output follows"
    );
}

#[test]
fn a_commit_someone_else_pushed_before_the_lookup_is_never_removed() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    let old = origin_tip(&fx);
    let theirs = push_from_other_clone(&fx);
    // GitHub now reports their commit as the PR's head.
    fx.cfg("pr-head", &format!("{theirs}\n"));
    pr_exists(&fx);
    amend_local(&fx);
    let edits = fx.calls_to("pr edit").len();
    let out = fx.run("T (#1)", "approved", "r");
    assert!(!out.status.success());
    assert!(stdout(&out).is_empty());
    assert_eq!(fx.calls_to("pr edit").len(), edits);
    assert_eq!(origin_tip(&fx), theirs);
    assert_eq!(
        first_line(&out),
        format!(
            "choco open-pr: someone else pushed to the task branch: the PR's head is {theirs} but this task last pushed {old}, and this branch doesn't contain it; bring their commits into the branch (git fetch, then merge), then resume"
        )
    );
}

#[test]
fn after_their_commit_is_merged_in_the_next_lap_succeeds() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    let theirs = push_from_other_clone(&fx);
    fx.cfg("pr-head", &format!("{theirs}\n"));
    pr_exists(&fx);
    amend_local(&fx);
    assert!(!fx.run("T (#1)", "approved", "r").status.success());
    assert_eq!(origin_tip(&fx), theirs);
    git(&fx.wt, &["fetch", "-q", "origin"]);
    git(
        &fx.wt,
        &["merge", "-q", "-m", "merge", "origin/task/branch-name-xyz"],
    );
    let out = fx.run("T (#1)", "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(origin_tip(&fx), git(&fx.wt, &["rev-parse", "HEAD"]).trim());
    // Their commit is still in the branch.
    git(
        &fx.root.join("origin.git"),
        &["merge-base", "--is-ancestor", &theirs, TASK_REF],
    );
    // The record now holds our new push, so the lease path works again.
    fx.cfg("pr-head", &format!("{}\n", origin_tip(&fx)));
    amend_local(&fx);
    let out = fx.run("T (#1)", "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(origin_tip(&fx), git(&fx.wt, &["rev-parse", "HEAD"]).trim());
}

#[test]
fn the_latest_record_wins_across_several_rebases() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    pr_exists(&fx);
    for lap in 0..3 {
        fx.cfg("pr-head", &format!("{}\n", origin_tip(&fx)));
        // A different tree each lap, so every amend makes a new commit.
        fs::write(fx.wt.join("f1"), format!("lap {lap}\n")).unwrap();
        git(&fx.wt, &["add", "."]);
        git(&fx.wt, &["commit", "-q", "--amend", "-m", "amended"]);
        let out = fx.run("T (#1)", "approved", "r");
        assert!(out.status.success(), "lap {lap}: {}", stderr(&out));
        assert_eq!(origin_tip(&fx), git(&fx.wt, &["rev-parse", "HEAD"]).trim());
    }
}

#[test]
fn the_lease_is_the_tasks_own_last_push_not_the_remote_tracking_ref() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    let old = origin_tip(&fx);
    fx.cfg("pr-head", &format!("{old}\n"));
    pr_exists(&fx);
    let theirs = push_from_other_clone(&fx);
    // The remote-tracking ref now equals their commit, so a bare lease would
    // overwrite it; the explicit lease on our own last push must refuse.
    git(&fx.wt, &["fetch", "-q", "origin"]);
    amend_local(&fx);
    let out = fx.run("T (#1)", "approved", "r");
    assert!(!out.status.success());
    assert!(stdout(&out).is_empty());
    assert_eq!(origin_tip(&fx), theirs);
    assert!(
        first_line(&out).contains("moved since the PR's head"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_open_pr_without_a_record_of_our_push_gets_a_plain_push() {
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    assert!(fx.run("T (#1)", "approved", "r").status.success());
    let old = origin_tip(&fx);
    fs::remove_file(fx.git_dir().join("choco-open-pr-pushed")).unwrap();
    fx.cfg("pr-head", &format!("{old}\n"));
    pr_exists(&fx);
    amend_local(&fx);
    let out = fx.run("T (#1)", "approved", "r");
    // A rebase cannot be force-pushed without our own record.
    assert!(!out.status.success());
    assert_eq!(origin_tip(&fx), old);
    assert!(
        first_line(&out).starts_with("choco open-pr: the push was rejected (non-fast-forward)")
    );
}

#[test]
fn a_ref_in_the_part_of_the_title_that_is_cut_still_gets_its_refs_line() {
    let title = format!("{} part of #77 tail", "a".repeat(120));
    let fx = Fixture::new();
    fx.write_description(b"d\n");
    let out = fx.run_closes(&title, Some(""), "approved", "r");
    assert!(out.status.success(), "{}", stderr(&out));
    let create = &fx.calls_to("pr create")[0];
    let body = String::from_utf8(create.body.clone().unwrap()).unwrap();
    assert!(body.contains("\nRefs #77\n"), "{body}");
    let arg = create
        .args
        .iter()
        .find_map(|a| a.strip_prefix(b"--title="))
        .unwrap();
    let t = std::str::from_utf8(arg).unwrap();
    assert!(!t.contains("#77"), "{t}");
}

// ---- hand edits inside choco's section are kept across laps (#344) ----

const HEADING: &str = "## Kept from choco's section: lines not in choco's last published copy";
const EXPLAIN: &str = "choco moved these lines here from its own section of this description: they were not in choco's last published copy (the saved copy it compares against) and are not in the new text, and that section is rewritten on every lap. choco never changes text outside its section, so edit or delete these freely.";
const TITLE: &str = "T (#1)";
const PREFIX: [&str; 2] = ["HUMAN PREFIX one", "HUMAN PREFIX two"];
const SUFFIX: [&str; 2] = ["HUMAN SUFFIX one", "HUMAN SUFFIX two"];
const DIE_TAIL: &str = " was published, but choco could not save a copy of it; the saved copy choco compares against on the next lap may be missing, out of date or incomplete, so the next lap may replace its section, or may move lines choco wrote itself";

fn moved_note(k: usize, n: u32) -> String {
    format!(
        "choco open-pr: note: moved {k} line(s) that were not in choco's last published copy or its new text, from inside choco's section of PR #{n} to below it, under \"{HEADING}\""
    )
}

fn no_copy_note(n: u32) -> String {
    format!(
        "choco open-pr: note: no saved copy of what choco last published in PR #{n}'s description (it may have been opened by an older choco, or the copy may be missing or incomplete): lines inside choco's section can't be compared, so the section was replaced and none were moved out"
    )
}

fn copy_path(fx: &Fixture) -> PathBuf {
    fx.git_dir().join("choco-open-pr-published.md")
}

fn lap(fx: &Fixture, desc: &str) -> Output {
    fx.write_description(desc.as_bytes());
    fx.run(TITLE, "approved", "r")
}

/// A real first lap: creates the PR (and so the real copy); returns the
/// recorded create body.
fn first_lap(fx: &Fixture, desc: &str) -> String {
    let out = lap(fx, desc);
    assert!(out.status.success(), "{}", stderr(&out));
    pr_body(fx)
}

/// A later lap over the live body `live`.
fn later_lap(fx: &Fixture, live: &str, desc: &str) -> Output {
    fx.cfg("open-number", "7\n");
    fx.cfg("body", live);
    lap(fx, desc)
}

fn last_edit_body(fx: &Fixture) -> String {
    let edits = fx.calls_to("pr edit");
    String::from_utf8(edits.last().expect("a pr edit").body.clone().unwrap()).unwrap()
}

/// The marker block (both marker lines included) of a body.
fn block_of(body: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for l in body.split_inclusive('\n') {
        let t = l.trim_end_matches(['\n', '\r', ' ', '\t']);
        if t == BEGIN {
            inside = true;
        }
        if inside {
            out.push_str(l);
        }
        if t == END {
            break;
        }
    }
    out
}

fn wrap(body: &str) -> String {
    format!(
        "{}\n{}\n{body}{}\n{}\n",
        PREFIX[0], PREFIX[1], SUFFIX[0], SUFFIX[1]
    )
}

fn lines_of(body: &str) -> Vec<&str> {
    body.split('\n').collect()
}

fn count_line(body: &str, line: &str) -> usize {
    body.split('\n').filter(|l| *l == line).count()
}

fn line_pos(body: &str, line: &str) -> Option<usize> {
    lines_of(body).iter().position(|l| *l == line)
}

fn assert_no_heading(body: &str) {
    assert_eq!(count_line(body, HEADING), 0, "{body}");
}

fn count_no_copy_notes(out: &Output) -> usize {
    stderr(out)
        .matches("choco open-pr: note: no saved copy of what choco ")
        .count()
}

fn count_moved_notes(out: &Output) -> usize {
    stderr(out).matches("choco open-pr: note: moved ").count()
}

const CI_LINE: &str =
    "- CI versions recorded by the operator: ubuntu-24.04 `git version 2.55.0` (`/usr/bin/git`)";

#[test]
fn a_hand_edit_inside_the_block_is_moved_below_it_and_kept_on_later_laps() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "keep me\nold line A\nold line D\n");
    let live = wrap(
        &body1
            .replace("old line A\n", "old line A, edited by hand\n")
            .replace("old line D\n", &format!("old line D\n{CI_LINE}\n")),
    );
    let out = later_lap(&fx, &live, "keep me\nnew line B\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);

    // (a)
    assert_eq!(count_line(&body2, HEADING), 1, "{body2}");
    // (b) after END, in order: heading, explanation, the two hand lines.
    let end = line_pos(&body2, END).unwrap();
    let h = line_pos(&body2, HEADING).unwrap();
    let e = line_pos(&body2, EXPLAIN).unwrap();
    let a = line_pos(&body2, "old line A, edited by hand").unwrap();
    let c = line_pos(&body2, CI_LINE).unwrap();
    assert!(end < h && h < e && e < a && a < c, "{body2}");
    // (c)
    assert!(
        !lines_of(&body2)[h..].contains(&"old line D"),
        "an unchanged line was moved: {body2}"
    );
    // (d)
    for l in PREFIX.iter().chain(SUFFIX.iter()) {
        assert_eq!(count_line(&body2, l), 1, "{l}: {body2}");
    }
    // (e)
    assert!(stderr(&out).contains(&moved_note(2, 7)), "{}", stderr(&out));
    // The negative-note helper must see a real note: exactly one here.
    assert_eq!(count_moved_notes(&out), 1, "{}", stderr(&out));
    // The moved lines are the last thing in the body, right under the text.
    assert!(
        body2.ends_with(&format!(
            "\n{HEADING}\n\n{EXPLAIN}\n\nold line A, edited by hand\n{CI_LINE}\n"
        )),
        "{body2}"
    );
    let section = &body2[body2.find(&format!("\n{HEADING}\n")).unwrap()..];

    // Lap 3 starts from lap 2's recorded body unchanged.
    let out = later_lap(&fx, &body2, "keep me\nnew line C\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body3 = last_edit_body(&fx);
    // (f)
    let h3 = line_pos(&body3, HEADING).expect("the moved section is kept");
    assert!(!lines_of(&body3)[h3..].contains(&"new line B"), "{body3}");
    // (g)
    assert!(body3.ends_with(section), "{body3}");
    assert_eq!(count_line(&body3, HEADING), 1);
    // (h)
    assert_eq!(count_moved_notes(&out), 0, "{}", stderr(&out));
    assert!(body3.contains("new line C") && !body3.contains("new line B"));
}

#[test]
fn a_hand_line_the_new_description_keeps_is_not_moved() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "alpha\n");
    let live = wrap(&body1.replace("alpha\n", "alpha\nX\n"));
    let out = later_lap(&fx, &live, "alpha\nX\nbeta\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_eq!(count_line(&body2, "X"), 1, "{body2}");
    assert_no_heading(&body2);
    assert_eq!(count_moved_notes(&out), 0, "{}", stderr(&out));
    assert!(body2.contains("\nbeta\n"));
    for l in PREFIX.iter().chain(SUFFIX.iter()) {
        assert_eq!(count_line(&body2, l), 1, "{body2}");
    }
}

#[test]
fn an_untouched_block_moves_nothing() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "old description only\nsecond old line\n");
    let live = wrap(&body1);
    let out = later_lap(&fx, &live, "brand new description\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_no_heading(&body2);
    assert_eq!(count_moved_notes(&out), 0, "{}", stderr(&out));
    assert!(!body2.contains("old description only") && !body2.contains("second old line"));
    assert!(body2.contains("\nbrand new description\n"));
    assert!(body2.starts_with(&format!("{}\n{}\n{BEGIN}\n", PREFIX[0], PREFIX[1])));
    assert!(body2.ends_with(&format!("{END}\n{}\n{}\n", SUFFIX[0], SUFFIX[1])));
}

#[test]
fn a_crlf_body_with_an_edit_outside_the_block_moves_nothing() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "old crlf line\n");
    let live = wrap(&body1)
        .replace(SUFFIX[0], "HUMAN SUFFIX one, edited outside")
        .replace('\n', "\r\n");
    assert!(live.contains("old crlf line\r\n") && live.contains(&format!("{END}\r\n")));
    let out = later_lap(&fx, &live, "new crlf line\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_no_heading(&body2);
    assert_eq!(count_moved_notes(&out), 0, "{}", stderr(&out));
    // The replacement happened, and the outside lines kept their CRLF bytes.
    assert!(body2.contains("\nnew crlf line\n") && !body2.contains("old crlf line"));
    assert!(body2.starts_with(&format!("{}\r\n{}\r\n{BEGIN}\n", PREFIX[0], PREFIX[1])));
    assert!(body2.ends_with(&format!(
        "{END}\nHUMAN SUFFIX one, edited outside\r\n{}\r\n",
        SUFFIX[1]
    )));
}

#[test]
fn without_a_copy_the_block_is_replaced_and_the_note_says_so() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "old line\n");
    fs::remove_file(copy_path(&fx)).unwrap();
    let live = wrap(&body1.replace("old line\n", "old line\nhand fact one\n"));
    let out = later_lap(&fx, &live, "second lap line\n");
    assert_no_heading(&last_edit_body(&fx));
    assert!(stderr(&out).contains(&no_copy_note(7)), "{}", stderr(&out));
    assert_eq!(count_no_copy_notes(&out), 1, "{}", stderr(&out));
    assert_eq!(count_line(&last_edit_body(&fx), "hand fact one"), 0);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert!(body2.contains("\nsecond lap line\n"));
    assert_eq!(
        fs::read_to_string(copy_path(&fx)).unwrap(),
        block_of(&body2)
    );

    // The baseline is saved now, so the next hand line is detected.
    let live3 = body2.replace("second lap line\n", "second lap line\nhand fact two\n");
    let out = later_lap(&fx, &live3, "third lap line\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body3 = last_edit_body(&fx);
    assert_eq!(count_line(&body3, HEADING), 1, "{body3}");
    assert!(body3.ends_with("hand fact two\n"), "{body3}");
    assert!(stderr(&out).contains(&moved_note(1, 7)));
    assert_eq!(count_no_copy_notes(&out), 0, "{}", stderr(&out));
}

#[test]
fn a_copy_that_cannot_be_saved_fails_the_stage() {
    // Create path: the copy path is a directory.
    let fx = Fixture::new();
    fs::create_dir(copy_path(&fx)).unwrap();
    let out = lap(&fx, "created text\n");
    assert!(!out.status.success());
    let creates = fx.calls_to("pr create");
    assert_eq!(creates.len(), 1, "the PR was published before the failure");
    let published = String::from_utf8(creates[0].body.clone().unwrap()).unwrap();
    assert!(published.contains("\ncreated text\n"));
    assert!(
        stderr(&out).contains(&format!(
            "choco open-pr: the new PR's description{DIE_TAIL}\n"
        )),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("PR #7"));
    assert_ne!(stdout(&out), READ_BACK);

    // Edit path: a real first lap, then the copy becomes a directory.
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "created text\n");
    fs::remove_file(copy_path(&fx)).unwrap();
    fs::create_dir(copy_path(&fx)).unwrap();
    let live = wrap(&body1);
    let out = later_lap(&fx, &live, "refreshed text\n");
    assert!(!out.status.success());
    assert_eq!(fx.calls_to("pr edit").len(), 1);
    assert!(last_edit_body(&fx).contains("\nrefreshed text\n"));
    assert!(
        stderr(&out).contains(&format!("choco open-pr: PR #7's description{DIE_TAIL}\n")),
        "{}",
        stderr(&out)
    );
    // The queued note survived the failure.
    assert!(stderr(&out).contains(&no_copy_note(7)), "{}", stderr(&out));
    assert_ne!(stdout(&out), READ_BACK);
}

#[test]
fn a_moved_line_cannot_close_an_issue() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "plain text\n");
    let live = wrap(&body1.replace("plain text\n", "plain text\nFixes #9\n"));
    let out = later_lap(&fx, &live, "other text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    let end = line_pos(&body2, END).unwrap();
    let moved = line_pos(&body2, "Fixes issue 9").expect("the moved line is rewritten");
    assert!(moved > end, "{body2}");
    assert!(!body2.contains("Fixes #9"), "{body2}");
    assert!(body2.starts_with(&format!("{}\n{}\n{BEGIN}\n", PREFIX[0], PREFIX[1])));
    for l in SUFFIX {
        assert_eq!(count_line(&body2, l), 1);
    }
}

#[test]
fn published_copies_track_only_successfully_published_generated_blocks() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "lap one text\n");
    let copy = copy_path(&fx);
    // The shell's idea of the private git dir agrees with the worktree's
    // `.git` pointer and with the filesystem.
    let pointer = fs::read_to_string(fx.wt.join(".git")).unwrap();
    let pointed = PathBuf::from(pointer.trim().strip_prefix("gitdir: ").unwrap());
    assert_eq!(pointed.canonicalize().unwrap(), fx.git_dir());
    assert!(fs::metadata(&copy).unwrap().is_file());
    assert_eq!(fs::read(&copy).unwrap(), block_of(&body1).into_bytes());

    let live2 = wrap(&body1);
    let out = later_lap(&fx, &live2, "lap two text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_eq!(fs::read(&copy).unwrap(), block_of(&body2).into_bytes());

    // A lap that moves a hand line: moved and outside text stay out of the copy.
    let live3 = body2.replace("lap two text\n", "lap two text\nhand only line\n");
    let out = later_lap(&fx, &live3, "lap three text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body3 = last_edit_body(&fx);
    assert!(body3.ends_with("hand only line\n"));
    let saved = fs::read_to_string(&copy).unwrap();
    assert_eq!(saved, block_of(&body3));
    assert!(!saved.contains("hand only line") && !saved.contains(HEADING));
    assert!(!saved.contains(PREFIX[0]) && !saved.contains(SUFFIX[1]));

    let out = later_lap(&fx, &body3, "lap four text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body4 = last_edit_body(&fx);
    assert_eq!(fs::read(&copy).unwrap(), block_of(&body4).into_bytes());
    assert_ne!(block_of(&body3), block_of(&body4));
}

#[test]
fn a_failed_edit_keeps_the_copy_and_a_retry_keeps_the_hand_line() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "first text\n");
    let before = fs::read(copy_path(&fx)).unwrap();
    let live = wrap(&body1.replace("first text\n", "first text\nhand line to keep\n"));
    fx.cfg("fail-pr-edit", "");
    let out = later_lap(&fx, &live, "second text\n");
    assert!(!out.status.success());
    assert_ne!(stdout(&out), READ_BACK);
    assert_eq!(fx.calls_to("pr edit").len(), 1, "the attempt was recorded");
    assert_eq!(fs::read(copy_path(&fx)).unwrap(), before);

    fs::remove_file(fx.root.join("gh/cfg/fail-pr-edit")).unwrap();
    let out = later_lap(&fx, &live, "second text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert!(
        body2.ends_with(&format!("\n{HEADING}\n\n{EXPLAIN}\n\nhand line to keep\n")),
        "{body2}"
    );
    assert!(stderr(&out).contains(&moved_note(1, 7)));
    assert_eq!(
        fs::read(copy_path(&fx)).unwrap(),
        block_of(&body2).into_bytes()
    );
    assert_ne!(fs::read(copy_path(&fx)).unwrap(), before);
}

#[test]
fn an_invalid_live_block_keeps_an_existing_copy_untouched() {
    let fx = Fixture::new();
    first_lap(&fx, "some text\n");
    let before = fs::read(copy_path(&fx)).unwrap();
    for live in [
        format!("outside\n{BEGIN}\nx\n{BEGIN}\ny\n{END}\nmore outside\n"),
        format!("outside\n{END}\nx\n{BEGIN}\nmore outside\n"),
        "outside text only\n".to_string(),
        format!("outside\n{BEGIN}\nhand line without an end\n"),
    ] {
        let out = later_lap(&fx, &live, "other text\n");
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert!(fx.calls_to("pr edit").is_empty());
        assert!(stderr(&out).contains("doesn't have exactly one choco block"));
        assert_eq!(count_moved_notes(&out), 0);
        assert_eq!(count_no_copy_notes(&out), 0, "{}", stderr(&out));
        assert_eq!(fs::read(copy_path(&fx)).unwrap(), before);
    }
}

#[test]
fn hand_line_membership_uses_whole_trimmed_lines_and_keeps_raw_output() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "kept old\nkept old two\nprefix of a line\n");
    let desc2 = "kept new\n";
    // The live block: old and new lines with trailing whitespace and CRs
    // (not true additions), marker lines with trailing whitespace, then the
    // true additions.
    let additions = [
        "  kept old",                    // leading indentation
        "kept  old two",                 // internal whitespace
        "KEPT OLD",                      // case
        "prefix of a line plus more \t", // shares only a prefix; trailing blanks kept
        "dup add",                       // two identical additions
        "between\r",                     // CR is dropped from the output
        "dup add  \r",
    ];
    let mut live_block = String::new();
    for l in block_of(&body1).split_inclusive('\n') {
        let t = l.trim_end_matches('\n');
        let out = match t {
            BEGIN => format!("{BEGIN}  \n"),
            END => format!("{END}\t\r\n"),
            "kept old" => "kept old  \t\n".to_string(),
            "kept old two" => "kept old two\r\n".to_string(),
            "" => "  \t\n".to_string(),
            "prefix of a line" => {
                let mut s = String::from("prefix of a line \r\n");
                for a in additions {
                    s.push_str(a);
                    s.push('\n');
                }
                s.push_str("kept new \t\n"); // equals a line of the new block
                s
            }
            _ => l.to_string(),
        };
        live_block.push_str(&out);
    }
    let live = wrap(&live_block);
    let out = later_lap(&fx, &live, desc2);
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    let expected: String = [
        "  kept old",
        "kept  old two",
        "KEPT OLD",
        "prefix of a line plus more \t",
        "dup add",
        "between",
        "dup add  ",
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect();
    assert!(
        body2.ends_with(&format!("\n{HEADING}\n\n{EXPLAIN}\n\n{expected}")),
        "{body2:?}"
    );
    assert_eq!(count_line(&body2, HEADING), 1);
    assert!(stderr(&out).contains(&moved_note(7, 7)), "{}", stderr(&out));
    assert_eq!(body2.matches(BEGIN).count(), 1);
    assert_eq!(body2.matches(END).count(), 1);
    assert_eq!(body2.matches("kept new").count(), 1, "{body2}");
}

#[test]
fn later_hand_moves_append_after_all_existing_outside_text() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "base text\n");
    let live2 = wrap(&body1.replace("base text\n", "base text\nhand one\nhand two\n"));
    let out = later_lap(&fx, &live2, "base text two\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert!(body2.ends_with("\nhand one\nhand two\n"));

    // The person edits one moved line (keeping text a filter would rewrite)
    // and deletes the other; a new hand line goes inside the block.
    let dir = fx.root.join("marks");
    fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    let nasty = format!("q\"uote 's' $(touch {d}/a) `touch {d}/b` ; touch {d}/c \\ %s \\n done");
    let live3 = body2
        .replace("hand one\n", "hand one, edited; Fixes #5 by the person\n")
        .replace("hand two\n", "")
        .replace("base text two\n", &format!("base text two\n{nasty}\n"));
    let out = later_lap(&fx, &live3, "base text three\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body3 = last_edit_body(&fx);
    // The person's outside edit stays byte for byte; the deletion is respected.
    assert_eq!(
        count_line(&body3, "hand one, edited; Fixes #5 by the person"),
        1
    );
    assert_eq!(count_line(&body3, "hand two"), 0, "{body3}");
    assert_eq!(count_line(&body3, HEADING), 2, "{body3}");
    assert_eq!(count_line(&body3, EXPLAIN), 2);
    // The new group is after all retained outside text.
    assert!(
        body3.ends_with(&format!("\n{HEADING}\n\n{EXPLAIN}\n\n{nasty}\n")),
        "{body3}"
    );
    let second = body3.rfind(&format!("\n{HEADING}\n")).unwrap();
    assert!(body3[..second].contains("hand one, edited; Fixes #5 by the person"));
    assert!(body3[..second].ends_with("by the person\n"), "{body3}");
    for marker in ["a", "b", "c"] {
        assert!(!dir.join(marker).exists(), "{marker} was created");
        assert!(!fx.wt.join(marker).exists());
    }
}

/// A fake `awk` first on PATH that fails on demand and otherwise runs the
/// real one. `fail-awk-hand` fails the hand-line comparison (the only awk
/// whose last argument is the live body); `fail-awk-moved` (holding a text)
/// fails any filter run whose input contains that text, after writing
/// partial output; `fail-awk-count` fails the count of the moved lines (the
/// only awk whose last argument is the moved-lines file).
fn install_fake_awk(fx: &Fixture) {
    let real = std::env::var("PATH")
        .unwrap()
        .split(':')
        .map(|d| Path::new(d).join("awk"))
        .find(|p| p.exists())
        .expect("a real awk");
    let script = format!(
        r#"#!/bin/sh
REAL='{}'
CFG="$GH_FAKE_DIR/cfg"
if [ -e "$CFG/fail-awk-hand" ]; then
    for a in "$@"; do last=$a; done
    case "$last" in
    */current)
        echo "INJECTED awk failure: hand-line comparison" >&2
        echo hit >> "$GH_FAKE_DIR/awk-hand-hits"
        exit 3
        ;;
    esac
fi
if [ -e "$CFG/fail-awk-count" ]; then
    for a in "$@"; do last=$a; done
    case "$last" in
    */hand)
        echo "INJECTED awk failure: moved-line count" >&2
        exit 3
        ;;
    esac
fi
if [ -e "$CFG/fail-awk-moved" ] && [ "$#" -eq 1 ]; then
    cat > "$GH_FAKE_DIR/awk-stdin"
    if grep -qF -- "$(cat "$CFG/fail-awk-moved")" "$GH_FAKE_DIR/awk-stdin"; then
        echo "PARTIAL OUTPUT MUST NOT BE PUBLISHED"
        echo "INJECTED awk failure: moved-line filter" >&2
        exit 3
    fi
    exec "$REAL" "$@" < "$GH_FAKE_DIR/awk-stdin"
fi
exec "$REAL" "$@"
"#,
        real.display()
    );
    let path = fx.root.join("bin/awk");
    fs::write(&path, script).unwrap();
    make_executable(&path);
}

/// Real first lap, then a live body with one hand line inside the block.
fn hand_line_setup(fx: &Fixture, hand: &str) -> (String, Vec<u8>) {
    let body1 = first_lap(fx, "first text\n");
    let live = wrap(&body1.replace("first text\n", &format!("first text\n{hand}\n")));
    (live, fs::read(copy_path(fx)).unwrap())
}

#[test]
fn a_failed_hand_line_comparison_aborts_before_edit() {
    let fx = Fixture::new();
    install_fake_awk(&fx);
    let (live, before) = hand_line_setup(&fx, "hand line kept");
    fx.cfg("fail-awk-hand", "");
    let out = later_lap(&fx, &live, "second text\n");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("INJECTED awk failure: hand-line comparison"),
        "{}",
        stderr(&out)
    );
    assert!(fx.root.join("gh/awk-hand-hits").exists());
    assert!(fx.calls_to("pr edit").is_empty());
    assert_eq!(fs::read(copy_path(&fx)).unwrap(), before);

    fs::remove_file(fx.root.join("gh/cfg/fail-awk-hand")).unwrap();
    let out = later_lap(&fx, &live, "second text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(last_edit_body(&fx).ends_with("\nhand line kept\n"));
}

#[test]
fn a_failed_moved_line_filter_aborts_before_edit() {
    let fx = Fixture::new();
    install_fake_awk(&fx);
    let (live, before) = hand_line_setup(&fx, "hand line kept, Fixes #9");
    fx.cfg("fail-awk-moved", "hand line kept");
    let out = later_lap(&fx, &live, "second text\n");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("INJECTED awk failure: moved-line filter"),
        "{}",
        stderr(&out)
    );
    assert!(fx.calls_to("pr edit").is_empty());
    assert_eq!(fs::read(copy_path(&fx)).unwrap(), before);

    fs::remove_file(fx.root.join("gh/cfg/fail-awk-moved")).unwrap();
    let out = later_lap(&fx, &live, "second text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body = last_edit_body(&fx);
    assert!(
        body.ends_with("\nhand line kept, Fixes issue 9\n"),
        "{body}"
    );
    assert!(!body.contains("PARTIAL OUTPUT") && !body.contains("Fixes #9"));
}

#[test]
fn a_failed_moved_line_count_aborts_before_edit() {
    let fx = Fixture::new();
    install_fake_awk(&fx);
    let (live, before) = hand_line_setup(&fx, "hand line kept");
    fx.cfg("fail-awk-count", "");
    let out = later_lap(&fx, &live, "second text\n");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("INJECTED awk failure: moved-line count"),
        "{}",
        stderr(&out)
    );
    assert!(fx.calls_to("pr edit").is_empty());
    assert_eq!(fs::read(copy_path(&fx)).unwrap(), before);

    fs::remove_file(fx.root.join("gh/cfg/fail-awk-count")).unwrap();
    let out = later_lap(&fx, &live, "second text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(last_edit_body(&fx).ends_with("\nhand line kept\n"));
    assert!(stderr(&out).contains(&moved_note(1, 7)), "{}", stderr(&out));
}

#[test]
fn trailing_blanks_in_the_saved_copy_do_not_make_a_line_a_hand_edit() {
    let fx = Fixture::new();
    // The published copy keeps the description's trailing blanks; the live
    // body (as GitHub or an editor may hand it back) has them stripped.
    let body1 = first_lap(&fx, "copy line  \t\ncopy tab\t \n");
    assert!(
        fs::read_to_string(copy_path(&fx))
            .unwrap()
            .contains("\ncopy line  \t\n")
    );
    let live = wrap(
        &body1
            .replace("copy line  \t\n", "copy line\n")
            .replace("copy tab\t \n", "copy tab\n"),
    );
    let out = later_lap(&fx, &live, "something else\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_no_heading(&body2);
    assert!(!stderr(&out).contains("moved "), "{}", stderr(&out));
    assert_eq!(count_line(&body2, "something else"), 1);
}

#[test]
fn trailing_blanks_in_the_new_block_do_not_make_a_line_a_hand_edit() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "first text\n");
    // The person's live line has no trailing blanks; the new description's
    // equal line does.
    let live = wrap(&body1.replace("first text\n", "first text\nnew line\nnew tab\n"));
    let out = later_lap(&fx, &live, "new line \t\nnew tab\t \n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_no_heading(&body2);
    assert!(!stderr(&out).contains("moved "), "{}", stderr(&out));
    assert_eq!(count_line(&body2, "new line \t"), 1);
    assert_eq!(count_line(&body2, "new line"), 0);
}

#[test]
fn a_failed_copy_write_flushes_the_moved_note() {
    let fx = Fixture::new();
    let (live, _) = hand_line_setup(&fx, "hand line kept");
    assert!(fs::metadata(copy_path(&fx)).unwrap().is_file());
    fx.cfg("copy-to-dir", copy_path(&fx).to_str().unwrap());
    let out = later_lap(&fx, &live, "second text\n");
    assert_eq!(
        fx.calls_to("pr edit").len(),
        1,
        "published before the failure"
    );
    assert!(last_edit_body(&fx).ends_with("\nhand line kept\n"));
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(&format!("choco open-pr: PR #7's description{DIE_TAIL}\n")),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains(&moved_note(1, 7)), "{}", stderr(&out));
    assert_eq!(count_no_copy_notes(&out), 0, "{}", stderr(&out));
}

/// A fake `cat` first on PATH that, while the control `partial-copy-write`
/// exists, writes only the first eight bytes of the block and fails: a copy
/// write that died partway. Otherwise it runs the real `cat`.
fn install_partial_cat(fx: &Fixture) {
    let real = std::env::var("PATH")
        .unwrap()
        .split(':')
        .map(|d| Path::new(d).join("cat"))
        .find(|p| p.exists())
        .expect("a real cat");
    let script = format!(
        r#"#!/bin/sh
if [ -e "$GH_FAKE_DIR/cfg/partial-copy-write" ] && [ "$#" -eq 1 ]; then
    case "$1" in
    */block)
        head -c 8 "$1"
        exit 1
        ;;
    esac
fi
exec '{}' "$@"
"#,
        real.display()
    );
    let path = fx.root.join("bin/cat");
    fs::write(&path, script).unwrap();
    make_executable(&path);
}

#[test]
fn a_partial_copy_after_a_failed_save_is_named_by_the_message_and_compared_against() {
    let fx = Fixture::new();
    install_partial_cat(&fx);
    let body1 = first_lap(&fx, "first text\n");
    fx.cfg("partial-copy-write", "");
    let out = later_lap(&fx, &wrap(&body1), "second text\n");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(&format!("choco open-pr: PR #7's description{DIE_TAIL}\n")),
        "{}",
        stderr(&out)
    );
    let published = last_edit_body(&fx);
    let partial = fs::read(copy_path(&fx)).unwrap();
    assert_eq!(partial.len(), 8, "a regular but incomplete copy remains");

    // The message's warning is true: the retry compares against what is there
    // and moves a line that only the coder wrote.
    fs::remove_file(fx.root.join("gh/cfg/partial-copy-write")).unwrap();
    let out = later_lap(&fx, &published, "third text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body3 = last_edit_body(&fx);
    assert!(body3.ends_with("\nsecond text\n"), "{body3}");
    assert!(stderr(&out).contains(&moved_note(1, 7)), "{}", stderr(&out));
}

#[test]
fn a_refused_copy_write_keeps_the_old_complete_copy_and_a_retry_compares_against_it() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "first text\n");
    let old = fs::read(copy_path(&fx)).unwrap();
    fs::set_permissions(copy_path(&fx), fs::Permissions::from_mode(0o444)).unwrap();
    let out = later_lap(&fx, &wrap(&body1), "second text\n");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(&format!("choco open-pr: PR #7's description{DIE_TAIL}\n")),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fx.calls_to("pr edit").len(),
        1,
        "published before the failure"
    );
    assert_eq!(
        fs::read(copy_path(&fx)).unwrap(),
        old,
        "the old copy is intact"
    );
    let published = last_edit_body(&fx);

    // The old copy is not missing, so no "no copy" note: the retry compares
    // against it and moves the coder's own line, as the message warns.
    fs::set_permissions(copy_path(&fx), fs::Permissions::from_mode(0o644)).unwrap();
    let out = later_lap(&fx, &published, "third text\n");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(count_no_copy_notes(&out), 0, "{}", stderr(&out));
    assert!(last_edit_body(&fx).ends_with("\nsecond text\n"));
}

#[test]
fn without_a_copy_a_live_line_the_new_text_has_stays_and_an_omitted_one_is_gone() {
    let fx = Fixture::new();
    let body1 = first_lap(&fx, "first text\n");
    fs::remove_file(copy_path(&fx)).unwrap();
    let live = wrap(&body1.replace(
        "first text\n",
        "first text\nretained hand fact\ndropped hand fact\n",
    ));
    let out = later_lap(&fx, &live, "first text\nretained hand fact\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let body2 = last_edit_body(&fx);
    assert_no_heading(&body2);
    assert_eq!(count_line(&body2, "retained hand fact"), 1, "{body2}");
    assert_eq!(count_line(&body2, "dropped hand fact"), 0, "{body2}");
    assert!(stderr(&out).contains(&no_copy_note(7)));
}
