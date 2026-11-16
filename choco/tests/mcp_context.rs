//! `choco mcp-serve`'s `get_context` binding as a real process (#369): what a
//! launch's flags do, what the server offers and refuses, and that only a
//! `get_context` call ever contacts the daemon. The daemon here is a private
//! fixture on an ephemeral port (never :4141) that counts its requests.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

const NAME: &str = "CHOCOFACTORY_SETTINGS_0123456789abcdef0123456789abcdef";
const VALUE: &str = "fedcba9876543210fedcba9876543210";

fn choco() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary has no path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let path = path.join("choco");
    assert!(
        path.exists(),
        "{} is missing: run `cargo build --workspace --all-targets` first",
        path.display()
    );
    path
}

/// A daemon fixture: answers every context request with a valid task answer
/// and counts the requests it gets.
struct Daemon {
    base: String,
    hits: Arc<AtomicUsize>,
}

async fn daemon() -> Daemon {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let counted = Arc::clone(&counted);
        async move {
            counted.fetch_add(1, Ordering::SeqCst);
            axum::Json(
                json!({"provenance": "task", "task_id": "task-1", "anchor_id": null,
                "task": {"title": "T", "input_available": true, "input": "I", "closes": []}}),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Daemon { base, hits }
}

fn binding_args(base: &str) -> Vec<String> {
    [
        "--context-task-id",
        "task-1",
        "--context-session-id",
        "session-1",
        "--context-base-url",
        base,
        "--context-no-anchor",
    ]
    .map(String::from)
    .to_vec()
}

struct Run {
    status: std::process::ExitStatus,
    replies: Vec<Value>,
    stderr: String,
}

/// Runs `choco mcp-serve args…`, sends `requests` and closes stdin.
fn serve(args: &[String], env: &[(&str, &str)], requests: &[Value]) -> Run {
    let mut command = Command::new(choco());
    command
        .arg("mcp-serve")
        .args(args)
        .env_remove(NAME)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for request in requests {
            // The server may already have exited on a bad flag.
            let _ = writeln!(stdin, "{request}");
        }
    }
    let output = child.wait_with_output().unwrap();
    Run {
        status: output.status,
        replies: String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn list() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
           "params": {"name": name, "arguments": arguments}})
}

fn names(reply: &Value) -> Vec<String> {
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

const REPORT: fn() -> Value = || {
    call(
        2,
        "report_outcome",
        json!({"outcome": "done", "summary": ""}),
    )
};
const READ: fn() -> Value = || call(3, "get_context", json!({"source": "task"}));

#[tokio::test(flavor = "multi_thread")]
async fn a_partial_or_unusable_binding_stops_the_server_before_it_serves() {
    let base = "http://127.0.0.1:4999";
    let full = binding_args(base);
    let without = |flag: &str| -> Vec<String> {
        let mut args = full.clone();
        let at = args.iter().position(|arg| arg == flag).unwrap();
        // The three value flags take a value; `--context-no-anchor` does not.
        args.drain(at..(at + if flag == "--context-no-anchor" { 1 } else { 2 }));
        args
    };
    let replace = |flag: &str, value: &str| -> Vec<String> {
        let mut args = full.clone();
        let at = args.iter().position(|arg| arg == flag).unwrap();
        args[at + 1] = value.to_string();
        args
    };
    let mut cases: Vec<(&str, Vec<String>)> = vec![
        ("no task", without("--context-task-id")),
        ("no session", without("--context-session-id")),
        ("no address", without("--context-base-url")),
        ("no anchor choice", without("--context-no-anchor")),
        ("blank task", replace("--context-task-id", " ")),
        ("blank session", replace("--context-session-id", "")),
        (
            "an address with no scheme",
            replace("--context-base-url", "127.0.0.1:4141"),
        ),
        (
            "a non-http address",
            replace("--context-base-url", "ftp://127.0.0.1"),
        ),
        (
            "an address with a query",
            replace("--context-base-url", "http://127.0.0.1:1/?a=b"),
        ),
        (
            "only the anchor flag",
            vec!["--context-no-anchor".to_string()],
        ),
        ("a misspelled flag", {
            let mut args = full.clone();
            args[0] = "--context-task".to_string();
            args
        }),
    ];
    let mut both = full.clone();
    both.extend(["--context-anchor-id".to_string(), "r1".to_string()]);
    cases.push(("both anchor choices", both));
    for (what, args) in cases {
        let run = serve(&args, &[], &[list()]);
        assert!(!run.status.success(), "{what}: should not serve");
        assert!(
            run.replies.is_empty(),
            "{what}: answered anyway: {:?}",
            run.replies
        );
        assert!(!run.stderr.trim().is_empty(), "{what}: no reason on stderr");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unbound_manual_mode_reports_without_a_daemon_and_refuses_get_context() {
    // Even with variables a client might use to find a daemon in the
    // environment, an unbound server routes nowhere.
    let daemon = daemon().await;
    let env = [
        ("CHOCO_BASE_URL", daemon.base.as_str()),
        ("CHOCOFACTORY_PORT", "1"),
    ];
    let run = serve(&[], &env, &[list(), REPORT(), READ()]);
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(names(&run.replies[0]), ["report_outcome"]);
    assert_eq!(run.replies[1]["result"]["isError"], json!(false));
    assert_eq!(run.replies[2]["result"]["isError"], json!(true));
    let text = run.replies[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("without a task binding"), "{text}");
    assert_eq!(daemon.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_server_contacts_the_daemon_only_for_a_context_call() {
    let daemon = daemon().await;
    let init = json!({"jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {}});
    let run = serve(
        &binding_args(&daemon.base),
        &[],
        &[
            init,
            list(),
            REPORT(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ],
    );
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(names(&run.replies[1]), ["report_outcome", "get_context"]);
    assert_eq!(run.replies[2]["result"]["isError"], json!(false));
    assert_eq!(
        daemon.hits.load(Ordering::SeqCst),
        0,
        "no request until a read"
    );
    let run = serve(&binding_args(&daemon.base), &[], &[READ()]);
    assert_eq!(run.replies[0]["result"]["isError"], json!(false));
    assert_eq!(daemon.hits.load(Ordering::SeqCst), 1);
    assert_eq!(run.stderr, "", "nothing is logged to stderr");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_server_whose_daemon_is_offline_still_lists_and_reports() {
    let args = binding_args("http://127.0.0.1:1");
    let run = serve(&args, &[], &[list(), REPORT(), READ()]);
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(names(&run.replies[0]), ["report_outcome", "get_context"]);
    assert_eq!(run.replies[1]["result"]["isError"], json!(false));
    // The read itself is the one thing that fails.
    assert_eq!(run.replies[2]["result"]["isError"], json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_settings_refuse_get_context_and_report_without_a_request() {
    let daemon = daemon().await;
    let mut args = binding_args(&daemon.base);
    args.extend([
        "--claude-settings-sentinel".to_string(),
        format!("{NAME}={VALUE}"),
    ]);
    for (what, env) in [
        ("a missing variable", None),
        ("a wrong value", Some((NAME, "something-else"))),
    ] {
        let env: Vec<(&str, &str)> = env.into_iter().collect();
        let run = serve(&args, &env, &[list(), READ(), REPORT()]);
        assert!(run.status.success(), "{what}: {}", run.stderr);
        assert_eq!(
            names(&run.replies[0]),
            ["claude_settings_rejected"],
            "{what}"
        );
        for reply in &run.replies[1..] {
            assert_eq!(reply["result"]["isError"], json!(true), "{what}");
            assert!(
                reply["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("did not apply the role's claude_settings"),
                "{what}: {reply}"
            );
        }
    }
    assert_eq!(
        daemon.hits.load(Ordering::SeqCst),
        0,
        "a refused call asked nobody"
    );
    // The right variable: both tools, and the read goes through.
    let run = serve(&args, &[(NAME, VALUE)], &[list(), READ()]);
    assert_eq!(names(&run.replies[0]), ["report_outcome", "get_context"]);
    assert_eq!(run.replies[1]["result"]["isError"], json!(false));
    assert_eq!(daemon.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn the_help_documents_the_binding_flags() {
    let output = Command::new(choco())
        .args(["mcp-serve", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--context-task-id",
        "--context-session-id",
        "--context-base-url",
        "--context-anchor-id",
        "--context-no-anchor",
        "--outcome",
        "--require-section",
        "--claude-settings-sentinel",
    ] {
        assert!(text.contains(flag), "{flag} missing from:\n{text}");
    }
}

/// Ctrl-C (SIGINT, as a terminal sends it) while a read is waiting on the
/// daemon ends the server, and leaves the daemon unaffected: the next server
/// reads and reports normally.
#[tokio::test(flavor = "multi_thread")]
async fn sigint_during_a_pending_read_ends_the_server_and_a_later_one_works() {
    let hold = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(AtomicUsize::new(0));
    let (gate, seen) = (Arc::clone(&hold), Arc::clone(&started));
    let app = axum::Router::new().fallback(move || {
        let (gate, seen) = (Arc::clone(&gate), Arc::clone(&seen));
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            gate.notified().await;
            axum::Json(
                json!({"provenance": "task", "task_id": "task-1", "anchor_id": null,
                "task": {"title": "T", "input_available": false, "input": null, "closes": []}}),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut child = Command::new(choco())
        .arg("mcp-serve")
        .args(binding_args(&base))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", READ()).unwrap();
    stdin.flush().unwrap();
    // The request has reached the daemon, so the server is mid-read.
    for _ in 0..600 {
        if started.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 1, "the read never started");
    let pid = child.id() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .unwrap()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");

    // The daemon (released now) serves the next server a read and a report.
    hold.notify_waiters();
    let args = binding_args(&base);
    let again = tokio::task::spawn_blocking(move || serve(&args, &[], &[REPORT(), READ()]));
    // `notify_waiters` only wakes current waiters: keep releasing while the
    // second read is in flight.
    let releaser = {
        let hold = Arc::clone(&hold);
        tokio::spawn(async move {
            for _ in 0..200 {
                hold.notify_waiters();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    };
    let run = again.await.unwrap();
    releaser.abort();
    assert_eq!(run.replies[0]["result"]["isError"], json!(false));
    assert_eq!(run.replies[1]["result"]["isError"], json!(false));
    assert!(run.status.success());
}
