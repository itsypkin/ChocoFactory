//! Runs the shipped `omp-probe.sh` against a fake `choco` that replays the
//! real event JSON of a probe task. Each case has its own fresh `TMPDIR` and
//! checks that nothing is left in it. Needs `sh`, `jq`, `git` and `tar`; a
//! missing one fails the test.

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const ANSWER: &str = "CANARY-DOTCLAUDE\nCANARY-ROOT-CLAUDE\nCANARY-ROOT-AGENTS\nCANARY-OMP-AGENTS\nCANARY-OMP-RULES\nno\nno";

/// `choco --json task events 405b9dbd-63e3-45a3-abe5-717d8aaa6795`, trimmed
/// to the fields the script reads plus the surrounding event types.
fn real_events() -> Value {
    let call = json!({
        "id": "3c930969-5a59-4965-be04-07977680fae9",
        "task_id": "405b9dbd-63e3-45a3-abe5-717d8aaa6795",
        "event_type": "tool_call",
        "payload": {
            "input": {"outcome": "done", "summary": ANSWER},
            "tool": "mcp__chocofactory__report_outcome",
            "tool_use_id": "call_ec9cb27ca4304c2aaa3d4fbc96443d61"
        }
    });
    json!({"events": [
        {"event_type": "stage_entered", "payload": {"kind": "agent_turn", "stage": "ask"}},
        {"event_type": "session_meta", "payload": {"model": "openai-codex/gpt-5.6-terra",
            "tools": ["read", "bash", "glob", "grep", "todo", "mcp__chocofactory__report_outcome"]}},
        {"event_type": "thinking", "payload": {"text": ""}},
        call,
        {"event_type": "tool_result", "payload": {"is_error": false,
            "output": "Recorded outcome 'done'.", "tool": "mcp__chocofactory__report_outcome"}},
        {"event_type": "assistant_message", "payload": {"text": ANSWER}},
        {"event_type": "turn_completed", "payload": {"is_error": false}},
        {"event_type": "stage_entered", "payload": {"kind": "terminal", "outcome": "done", "stage": "done"}}
    ], "next_token": null})
}

const FAKE: &str = r#"#!/bin/sh
d=$(dirname "$0")
echo "$*" >>"$d/log"
case "$*" in
*"task create"*)
    while [ "$#" -gt 0 ]; do
        [ "$1" = --repo ] && repo=$2
        shift
    done
    echo "$repo" >"$d/repo_arg"
    find "$repo" | sort >"$d/listing"
    echo '{"id": "T1", "status": "queued"}'
    if [ -f "$d/block_create" ]; then
        echo $$ >"$d/pid"
        : >"$d/blocked"
        exec sleep 600
    fi
    ;;
*"task status"*)
    if [ -f "$d/block_watch" ]; then
        echo $$ >"$d/pid"
        : >"$d/blocked"
        exec sleep 600
    fi
    exit "$(cat "$d/watch_code")"
    ;;
*"task events"*)
    cat "$d/events.json"
    ;;
*"task cancel"*)
    if [ -d "$(cat "$d/repo_arg")" ]; then echo "cancel-saw-folder=yes" >>"$d/log"
    else echo "cancel-saw-folder=no" >>"$d/log"; fi
    ;;
esac
"#;

struct Case {
    dir: PathBuf,
    tmp: PathBuf,
    repo: PathBuf,
    commit: String,
}

fn sh(dir: &Path, prog: &str, args: &[&str]) -> String {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("{prog} must be installed: {e}"));
    assert!(out.status.success(), "{prog} {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

impl Case {
    fn new(watch_code: i32, events: &Value) -> Case {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "omp-probe-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let tmp = dir.join("tmp");
        let repo = dir.join("repo");
        fs::create_dir_all(&tmp).unwrap();
        fs::create_dir_all(repo.join(".omp")).unwrap();
        fs::write(repo.join(".omp/AGENTS.md"), "CANARY-OMP-AGENTS\n").unwrap();
        fs::write(repo.join("README.md"), "root\n").unwrap();
        sh(&repo, "git", &["init", "-q"]);
        sh(&repo, "git", &["add", "-A"]);
        sh(
            &repo,
            "git",
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "c",
            ],
        );
        let commit = sh(&repo, "git", &["rev-parse", "HEAD"]);
        let fake = dir.join("choco");
        fs::write(&fake, FAKE).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("watch_code"), watch_code.to_string()).unwrap();
        fs::write(dir.join("events.json"), events.to_string()).unwrap();
        Case {
            dir,
            tmp,
            repo,
            commit,
        }
    }

    fn command(&self) -> Command {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.claude/skills/run-choco-task/scripts/omp-probe.sh");
        assert!(script.is_file(), "script missing: {}", script.display());
        let mut c = Command::new("sh");
        c.arg(script)
            .args(["proj", self.repo.to_str().unwrap(), &self.commit, "m", "q?"])
            .env("CHOCO", self.dir.join("choco"))
            .env("TMPDIR", &self.tmp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn run(&self) -> Output {
        self.command().output().unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    fn assert_tmp_empty(&self) {
        let left: Vec<_> = fs::read_dir(&self.tmp).unwrap().collect();
        assert!(left.is_empty(), "TMPDIR not empty: {left:?}");
    }

    /// Start the script with the fake blocking in `which`, wait for the fake's
    /// marker file, send TERM to the script and then to the blocked fake (a
    /// group signal would reach both). A watchdog kills the script after 20 s.
    fn run_terminated(&self, which: &str) -> Output {
        fs::write(self.dir.join(which), "").unwrap();
        let child = self.command().spawn().unwrap();
        let pid = child.id() as i32;
        let marker = self.dir.join("blocked");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !marker.exists() {
            if Instant::now() > deadline {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("fake never reached the blocking call");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if which == "block_create" {
            let fake_pid: i32 = fs::read_to_string(self.dir.join("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            unsafe { libc::kill(fake_pid, libc::SIGTERM) };
        }
        let mut child = child;
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                // Failure guard: the script did not end on TERM.
                if let Ok(p) = fs::read_to_string(self.dir.join("pid")) {
                    unsafe { libc::kill(p.trim().parse().unwrap(), libc::SIGKILL) };
                }
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("script did not exit after SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn report_only_prints_answer_and_does_not_cancel_a_closed_task() {
    let c = Case::new(6, &real_events());
    let o = c.run();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{ANSWER}\n"));
    assert!(
        !c.log().contains("task cancel"),
        "closed task was cancelled"
    );
    let listing = fs::read_to_string(c.dir.join("listing")).unwrap();
    assert!(listing.contains(".omp/AGENTS.md") && listing.contains("README.md"));
    c.assert_tmp_empty();
}

#[test]
fn another_tool_call_exits_3_after_printing_the_answer() {
    let mut ev = real_events();
    let list = ev["events"].as_array_mut().unwrap();
    list.push(json!({"event_type": "tool_call", "payload": {"tool": "read", "input": {}}}));
    let c = Case::new(6, &ev);
    let o = c.run();
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(stdout(&o), format!("{ANSWER}\n"));
    assert!(stderr(&o).contains("read"));
    assert!(!c.log().contains("task cancel"));
    c.assert_tmp_empty();
}

#[test]
fn next_token_is_refused_with_nothing_on_stdout() {
    let mut ev = real_events();
    ev["next_token"] = json!("more");
    let c = Case::new(6, &ev);
    let o = c.run();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    c.assert_tmp_empty();
}

#[test]
fn unfinished_task_is_cancelled_before_the_folder_is_removed() {
    for code in [5, 3] {
        let c = Case::new(code, &real_events());
        let o = c.run();
        assert_eq!(o.status.code(), Some(1));
        assert_eq!(stdout(&o), "");
        let log = c.log();
        assert!(log.contains("task cancel T1"), "watch {code}: {log}");
        assert!(log.contains("cancel-saw-folder=yes"), "watch {code}: {log}");
        c.assert_tmp_empty();
    }
}

#[test]
fn signal_during_task_create_cancels_the_created_task() {
    let c = Case::new(6, &real_events());
    let o = c.run_terminated("block_create");
    assert_eq!(o.status.code(), Some(143), "{}", stderr(&o));
    let log = c.log();
    assert!(log.contains("task cancel T1"), "{log}");
    assert!(log.contains("cancel-saw-folder=yes"), "{log}");
    c.assert_tmp_empty();
}

#[test]
fn signal_during_the_watch_cancels_and_cleans_up() {
    let c = Case::new(6, &real_events());
    let o = c.run_terminated("block_watch");
    assert_eq!(o.status.code(), Some(143), "{}", stderr(&o));
    let log = c.log();
    assert!(log.contains("task cancel T1"), "{log}");
    assert!(log.contains("cancel-saw-folder=yes"), "{log}");
    c.assert_tmp_empty();
}
