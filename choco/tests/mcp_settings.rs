//! `choco mcp-serve --claude-settings-sentinel` as a real process (#346): the
//! server offers `report_outcome` only when the variable the daemon put in the
//! role's settings `env` is present in the server's own environment with the
//! expected value.

use std::io::Write;
use std::process::{Command, Stdio};

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

/// Runs `choco mcp-serve <flag>` with `env` added, sends `tools/list`, and
/// returns the tool names plus the stderr text.
fn tools(flag: &str, env: Option<(&str, &str)>) -> (std::process::ExitStatus, Vec<String>, String) {
    let mut command = Command::new(choco());
    command
        .args([
            "mcp-serve",
            "--outcome",
            "done",
            "--claude-settings-sentinel",
            flag,
        ])
        .env_remove(NAME)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((name, value)) = env {
        command.env(name, value);
    }
    let mut child = command.spawn().unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        // The child may already have exited on a bad flag; that is the case
        // under test, so a broken pipe is fine.
        let _ = writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list"}}"#);
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let names = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .flat_map(|reply| {
            reply["result"]["tools"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    (
        output.status,
        names,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_variable_with_the_right_value_leaves_report_outcome_offered() {
    let (status, names, _) = tools(&format!("{NAME}={VALUE}"), Some((NAME, VALUE)));
    assert!(status.success());
    assert_eq!(names, vec!["report_outcome"]);
}

#[test]
fn a_missing_variable_offers_only_the_rejected_tool() {
    let (status, names, _) = tools(&format!("{NAME}={VALUE}"), None);
    assert!(status.success());
    assert_eq!(names, vec!["claude_settings_rejected"]);
}

#[test]
fn a_variable_with_a_different_value_offers_only_the_rejected_tool() {
    let (status, names, _) = tools(&format!("{NAME}={VALUE}"), Some((NAME, "something-else")));
    assert!(status.success());
    assert_eq!(names, vec!["claude_settings_rejected"]);
}

#[test]
fn a_malformed_flag_exits_non_zero_and_stderr_does_not_repeat_it() {
    for flag in ["NOEQUALS-secret", "OTHER_NAME=secret-value"] {
        let (status, names, stderr) = tools(flag, None);
        assert!(!status.success(), "{flag}");
        assert!(names.is_empty(), "{flag}: {names:?}");
        assert!(!stderr.is_empty(), "{flag}: the reason goes to stderr");
        assert!(
            !stderr.contains("secret") && !stderr.contains(flag),
            "{flag}: {stderr}"
        );
    }
}
