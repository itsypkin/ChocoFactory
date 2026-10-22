//! Tests for the omp adapter, against `tests/fixtures/fake_omp.py`.
//!
//! The fake is configured through a per-test wrapper script that exports its
//! environment variables, never through this process's own environment, so
//! tests running in parallel can't affect each other.

use super::*;
use crate::adapter::{ClaudeAdapter, TokenCounts};
use tokio::io::AsyncWriteExt;

fn fixture_binary(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omp-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A wrapper script exporting `env`, then running `fake_omp.py`.
fn wrapper(dir: &Path, env: &[(&str, &str)]) -> String {
    wrapper_for(dir, "fake_omp.py", env, None)
}

fn wrapper_for(dir: &Path, fixture: &str, env: &[(&str, &str)], marker: Option<&Path>) -> String {
    // The first arm lets `wrapper_for` run the script once (below) without
    // touching the marker or the fixture.
    let mut script = String::from("#!/bin/sh\n[ \"$1\" = --choco-warm-up ] && exit 0\n");
    if let Some(marker) = marker {
        script.push_str(&format!("touch '{}'\n", marker.display()));
    }
    for (key, value) in env {
        script.push_str(&format!(
            "export {key}='{}'\n",
            value.replace('\'', "'\\''")
        ));
    }
    // Answer `--version` here: the adapter probes it with a 5 s timeout, and
    // starting the Python fixture just to print a constant is what blew that
    // budget under load. Mirrors `fake_omp.py`'s own `--version` branch.
    script.push_str(
        "case \"$1\" in\n--version)\n    if [ -n \"${FAKE_OMP_VERSION_FAIL:-}\" ]; then exit 1; fi\n    if [ -n \"${FAKE_OMP_VERSION_LOG:-}\" ]; then printf '%s\\n' \"${XDG_STATE_HOME:-}\" >> \"$FAKE_OMP_VERSION_LOG\"; if [ -d \"${XDG_STATE_HOME:-/nonexistent}/omp\" ]; then echo yes; else echo no; fi >> \"$FAKE_OMP_VERSION_LOG\"; fi\n    echo 'omp/fake-1.0'\n    exit 0\n    ;;\nesac\n",
    );
    script.push_str(&format!("exec '{}' \"$@\"\n", fixture_binary(fixture)));
    let path = dir.join(format!("wrapper-{}", uuid::Uuid::new_v4()));
    crate::test_support::write_executable(&path, script);
    // On macOS the first exec of a freshly written file pays a one-time
    // Gatekeeper check (0.3 s to over 5 s under load). Pay it here, so the
    // adapter's 5 s `--version` probe never includes it. A real omp binary
    // has been run before and doesn't pay it per probe.
    let status = std::process::Command::new(&path)
        .arg("--choco-warm-up")
        .status()
        .unwrap_or_else(|e| panic!("warming up {}: {e}", path.display()));
    assert!(
        status.success(),
        "warming up {} failed: {status}",
        path.display()
    );
    path.to_string_lossy().into_owned()
}

struct Env {
    dir: PathBuf,
    state: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempdir("t");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        Self {
            state: dir.join("state"),
            repo,
            dir,
        }
    }

    fn adapter(&self, env: &[(&str, &str)]) -> OmpAdapter {
        OmpAdapter::with_binary(wrapper(&self.dir, env), &self.state)
    }

    fn cfg(&self) -> RoleConfig {
        RoleConfig {
            role: "coder".to_string(),
            project_id: None,
            cwd: self.repo.clone(),
            model: Some("openai-codex/gpt-5.6-terra".to_string()),
            system_prompt: None,
            sandboxed: true,
            report_outcomes: vec!["done".to_string(), "failed".to_string()],
            report_sections: Vec::new(),
            isolation: Isolation::default(),
            disallowed_tools: Vec::new(),
        }
    }

    /// A `memory: true` role of project `project`.
    fn memory_cfg(&self, project: &str) -> RoleConfig {
        RoleConfig {
            isolation: Isolation::Isolated {
                skills: Vec::new(),
                memory: true,
            },
            project_id: Some(project.to_string()),
            ..self.cfg()
        }
    }
}

/// Events up to and including the first `TurnCompleted`.
async fn until_turn_completed(handle: &mut AgentHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), handle.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("the stream ended before a TurnCompleted");
        let done = matches!(event, AgentEvent::TurnCompleted { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

/// Every remaining event once stdin is closed, until the stream ends.
async fn drain(handle: &mut AgentHandle) -> Vec<AgentEvent> {
    handle.close_stdin();
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), handle.recv())
        .await
        .expect("timed out draining")
    {
        events.push(event);
    }
    events
}

fn usage_of(events: &[AgentEvent]) -> TurnUsage {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnCompleted { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("no TurnCompleted")
}

/// The JSON report the fake's `echo` mode puts in its final message.
fn echo_report(events: &[AgentEvent]) -> Value {
    let text = events
        .iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::AssistantMessage { text } => Some(text.clone()),
            _ => None,
        })
        .expect("no assistant message");
    serde_json::from_str(&text).expect("the echo report is JSON")
}

async fn echo_run(env: &Env, cfg: &RoleConfig) -> Value {
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "echo,noreport")]);
    let mut handle = adapter.start("hi", cfg).unwrap();
    let events = until_turn_completed(&mut handle).await;
    echo_report(&events)
}

fn argv_of(report: &Value) -> Vec<String> {
    report["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect()
}

fn arg_after(argv: &[String], name: &str) -> Option<String> {
    argv.iter()
        .position(|arg| arg == name)
        .map(|index| argv[index + 1].clone())
}

fn overlay_of(report: &Value) -> Value {
    let overlay: Value = serde_yaml::from_str(report["overlay"].as_str().expect("overlay text"))
        .expect("the overlay is YAML");
    overlay
}

// ---------------------------------------------------------------------------
// The turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_report_completes_the_turn_with_the_captured_outcome() {
    let env = Env::new();
    let adapter = env.adapter(&[(
        "FAKE_OMP_REPORTS",
        r#"[{"outcome": "failed", "summary": "because"}]"#,
    )]);
    let mut handle = adapter.start("do it", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;

    let AgentEvent::SessionMeta {
        adapter_session_id,
        details,
    } = &events[0]
    else {
        panic!("first event should be SessionMeta: {events:?}");
    };
    assert_eq!(adapter_session_id, "omp-session-1");
    assert_eq!(details["model"], "openai-codex/gpt-5.6-terra");
    assert_eq!(details["omp_version"], "omp/fake-1.0");
    assert_eq!(details["isolation"], env.cfg().isolation.describe());
    let tools: Vec<&str> = details["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool.as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        vec![
            "read",
            "bash",
            "edit",
            "write",
            "glob",
            "grep",
            "todo",
            "mcp__chocofactory__report_outcome"
        ]
    );
    assert!(!tools.contains(&"report_outcome"));

    // Tool correlation, thinking, and the report as a ToolCall/ToolResult
    // pair under the qualified name with the arguments verbatim.
    let qualified = qualified_report_outcome_tool_name();
    assert!(events.contains(&AgentEvent::Thinking {
        text: "thinking it over".to_string()
    }));
    assert!(events.contains(&AgentEvent::ToolCall {
        tool_use_id: "call-read".to_string(),
        tool: "read".to_string(),
        input: json!({"path": "a.txt"}),
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        tool_use_id: "call-read".to_string(),
        tool: "read".to_string(),
        output: "file body".to_string(),
        is_error: false,
    }));
    let calls: Vec<&AgentEvent> = events
        .iter()
        .filter(|event| {
            matches!(event, AgentEvent::ToolCall { tool, .. } | AgentEvent::ToolResult { tool, .. } if tool.contains("report_outcome"))
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            &AgentEvent::ToolCall {
                tool_use_id: "call-report-0".to_string(),
                tool: qualified.clone(),
                input: json!({"outcome": "failed", "summary": "because"}),
            },
            &AgentEvent::ToolResult {
                tool_use_id: "call-report-0".to_string(),
                tool: qualified,
                output: "Recorded outcome 'failed'.".to_string(),
                is_error: false,
            },
        ],
        "the message_end copies of the report call and result produce no events"
    );
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_rejected_report_is_an_error_result_and_the_retry_is_accepted() {
    for (reports, rejected_text, sections) in [
        (
            r#"[{"outcome": "bogus", "summary": ""}, {"outcome": "done", "summary": ""}]"#,
            "'bogus' is not a valid outcome",
            Vec::new(),
        ),
        (
            r#"[{"outcome": "done", "summary": "nothing"}, {"outcome": "done", "summary": "Findings: none"}]"#,
            "missing 'Findings'",
            vec!["Findings".to_string()],
        ),
    ] {
        let env = Env::new();
        let adapter = env.adapter(&[("FAKE_OMP_REPORTS", reports)]);
        let mut cfg = env.cfg();
        cfg.report_sections = sections;
        let mut handle = adapter.start("go", &cfg).unwrap();
        let events = until_turn_completed(&mut handle).await;
        let results: Vec<(bool, String)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolResult {
                    tool,
                    is_error,
                    output,
                    ..
                } if tool.contains("report_outcome") => Some((*is_error, output.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(
            results[0].0 && results[0].1.contains(rejected_text),
            "{results:?}"
        );
        assert!(!results[1].0, "{results:?}");
        drain(&mut handle).await;
    }
}

#[tokio::test]
async fn the_third_section_missing_call_is_accepted_with_the_gap_message() {
    let env = Env::new();
    let thin = r#"{"outcome": "done", "summary": "thin"}"#;
    let adapter = env.adapter(&[("FAKE_OMP_REPORTS", &format!("[{thin},{thin},{thin}]"))]);
    let mut cfg = env.cfg();
    cfg.report_sections = vec!["Findings".to_string()];
    let mut handle = adapter.start("go", &cfg).unwrap();
    let events = until_turn_completed(&mut handle).await;
    let results: Vec<(bool, String)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult {
                tool,
                is_error,
                output,
                ..
            } if tool.contains("report_outcome") => Some((*is_error, output.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results[0].0 && results[1].0);
    assert!(!results[2].0);
    assert!(
        results[2].1.contains("still missing 'Findings'"),
        "{results:?}"
    );
    drain(&mut handle).await;
}

#[tokio::test]
async fn send_is_a_follow_up_prompt_and_the_first_prompt_is_not() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport")]);
    let mut handle = adapter.start("first", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        text: "echo:first|None".to_string()
    }));
    handle.send("second").unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        text: "echo:second|followUp".to_string()
    }));
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_u2028_inside_a_json_string_survives_the_line_reader() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,u2028")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        text: "before\u{2028}after".to_string()
    }));
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_chunked_frame_is_reassembled() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,chunked")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        text: "echo:go|None".to_string()
    }));
    drain(&mut handle).await;
}

#[tokio::test]
async fn malformed_frames_and_bad_chunk_sequences_are_skipped() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,garbage,bad_chunk")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(events.contains(&AgentEvent::AssistantMessage {
        text: "echo:go|None".to_string()
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
    drain(&mut handle).await;
}

#[tokio::test]
async fn dying_mid_turn_closes_the_stream_without_a_turn_completed() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,die")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), handle.recv())
        .await
        .unwrap()
    {
        events.push(event);
    }
    assert!(events.contains(&AgentEvent::AssistantMessage {
        text: "about to die".to_string()
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn prompt_result_errors_become_error_or_interrupted_then_a_failed_turn() {
    type EventCheck = fn(&AgentEvent) -> bool;
    let cases: [(&str, Option<&str>, EventCheck); 3] = [
        (
            "provider exploded",
            None,
            |e| matches!(e, AgentEvent::Error { message } if message == "provider exploded"),
        ),
        (
            "slow down",
            Some("429"),
            |e| matches!(e, AgentEvent::Interrupted { message, detected_by: InterruptionEvidence::Structured } if message == "slow down"),
        ),
        ("You've hit your usage limit", None, |e| {
            matches!(
                e,
                AgentEvent::Interrupted {
                    detected_by: InterruptionEvidence::MessageText,
                    ..
                }
            )
        }),
    ];
    for (message, status, matches_event) in cases {
        let env = Env::new();
        let mut vars = vec![
            ("FAKE_OMP_MODES", "noreport,error"),
            ("FAKE_OMP_ERROR_MESSAGE", message),
        ];
        if let Some(status) = status {
            vars.push(("FAKE_OMP_ERROR_STATUS", status));
        }
        let adapter = env.adapter(&vars);
        let mut handle = adapter.start("go", &env.cfg()).unwrap();
        let events = until_turn_completed(&mut handle).await;
        let n = events.len();
        assert!(matches!(
            events[n - 1],
            AgentEvent::TurnCompleted { is_error: true, .. }
        ));
        assert!(matches_event(&events[n - 2]), "{events:?}");
        // Exactly one of the three, not several.
        let flagged = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Error { .. } | AgentEvent::Interrupted { .. }))
            .count();
        assert_eq!(flagged, 1, "{events:?}");
        drain(&mut handle).await;
    }
}

#[tokio::test]
async fn an_errored_turn_completes_without_waiting_for_the_session_to_settle() {
    let env = Env::new();
    // The fake never sends `session_settled` after an error.
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,error,unsettled")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted { is_error: true, .. })
    ));
}

#[tokio::test]
async fn an_unsettled_session_completes_on_session_settled() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unsettled")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    // The follow-up run that background work started streams before the
    // session settles, so it comes before the turn completes.
    assert!(
        matches!(
            &events[events.len() - 2..],
            [
                AgentEvent::AssistantMessage { text },
                AgentEvent::TurnCompleted { is_error: false, .. },
            ] if text == "background result"
        ),
        "{events:?}"
    );
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_builtin_command_completes_on_its_response() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "local_command")]);
    let mut handle = adapter.start("/help", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
}

#[tokio::test]
async fn a_turn_completes_without_figures_when_stdin_was_closed_first() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    handle.close_stdin();
    let events = until_turn_completed(&mut handle).await;
    assert_eq!(usage_of(&events).tokens, TokenCounts::default());
}

#[tokio::test]
async fn a_report_whose_reply_cannot_be_sent_is_not_recorded_as_accepted() {
    let env = Env::new();
    let adapter = env.adapter(&[]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    handle.close_stdin();
    // The fake exits on the closed stdin without a reply: read to the end.
    let mut events = Vec::new();
    while let Some(event) = handle.recv().await {
        events.push(event);
    }
    let result = events.iter().find_map(|event| match event {
        AgentEvent::ToolResult { tool, is_error, .. } if tool.contains("report_outcome") => {
            Some(*is_error)
        }
        _ => None,
    });
    assert_eq!(result, Some(true), "{events:?}");
}

#[tokio::test]
async fn background_messages_count_toward_the_turn_that_waited_for_them() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unsettled")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert_eq!(usage_of(&events).model_turns, Some(3));
}

#[tokio::test]
async fn an_unknown_host_tool_is_answered_as_an_error() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unknown_tool")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    let reply = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::AssistantMessage { text } => text.strip_prefix("unknown-tool-reply:"),
            _ => None,
        })
        .expect("the fake saw the daemon's reply");
    let reply: Value = serde_json::from_str(reply).unwrap();
    assert_eq!(reply["id"], "htc-unknown");
    assert_eq!(reply["isError"], true);
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(
        reply["result"]["content"][0]["text"],
        "unknown host tool 'mystery'"
    );
    // It is not reported as a report_outcome call.
    assert!(!events.iter().any(
        |event| matches!(event, AgentEvent::ToolCall { tool, .. } if tool.contains("mystery"))
    ));
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_prompt_result_for_a_prompt_nobody_sent_completes_nothing() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,stray_result")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let mut events = until_turn_completed(&mut handle).await;
    events.extend(drain(&mut handle).await);
    let completed = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
        .count();
    assert_eq!(completed, 1, "{events:?}");
    // The stray result's figures were not spent on the real turn.
    assert_eq!(usage_of(&events).model_turns, Some(2));
}

#[tokio::test]
async fn stdout_ending_while_the_statistics_are_awaited_completes_the_turn_then_flushes() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,exit_on_stats")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let mut events = until_turn_completed(&mut handle).await;
    assert_eq!(usage_of(&events).tokens, TokenCounts::default());
    events.extend(drain(&mut handle).await);
    let completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
        .unwrap();
    let late = events
        .iter()
        .position(|event| {
            *event
                == AgentEvent::AssistantMessage {
                    text: "late text".to_string(),
                }
        })
        .expect("the event produced while waiting is flushed, not lost");
    assert!(late > completed, "buffered events follow the TurnCompleted");
}

#[tokio::test]
async fn a_process_that_exits_before_the_session_settles_still_completes_the_finished_turn() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unsettled,unsettled_die")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
    assert_eq!(usage_of(&events).tokens, TokenCounts::default());
    assert_eq!(usage_of(&events).model_turns, Some(2));
}

#[tokio::test]
async fn a_failed_get_state_or_set_host_tools_ends_the_session_with_an_error() {
    for (mode, expected) in [
        ("state_error", "get_state failed: state unavailable"),
        (
            "tools_error",
            "set_host_tools failed: cannot register tools",
        ),
    ] {
        let env = Env::new();
        let adapter = env.adapter(&[("FAKE_OMP_MODES", mode)]);
        let mut handle = adapter.start("go", &env.cfg()).unwrap();
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(30), handle.recv())
            .await
            .unwrap()
        {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error { message } if message == expected)),
            "{mode}: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
        );
    }
}

#[tokio::test]
async fn protocol_v2_is_only_negotiated_when_offered_and_a_failure_keeps_v1() {
    for modes in ["noreport,v1_only", "noreport,chunked,negotiate_error"] {
        let env = Env::new();
        let adapter = env.adapter(&[("FAKE_OMP_MODES", modes)]);
        let mut handle = adapter.start("go", &env.cfg()).unwrap();
        let events = until_turn_completed(&mut handle).await;
        assert!(
            events.contains(&AgentEvent::AssistantMessage {
                text: "echo:go|None".to_string()
            }),
            "{modes}: {events:?}"
        );
        drain(&mut handle).await;
    }
}

/// The wrapper answers `--version` itself: pointed at a fixture that does not
/// exist, only the wrapper's own arm can produce this output.
#[test]
fn the_wrapper_answers_version_without_starting_the_fixture() {
    let env = Env::new();
    let wrapper = wrapper_for(&env.dir, "no-such-fixture.py", &[], None);
    let out = std::process::Command::new(&wrapper)
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "omp/fake-1.0");

    let failing = wrapper_for(
        &env.dir,
        "no-such-fixture.py",
        &[("FAKE_OMP_VERSION_FAIL", "1")],
        None,
    );
    let out = std::process::Command::new(&failing)
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty());
}

#[tokio::test]
async fn the_version_is_null_when_the_version_command_fails() {
    let env = Env::new();
    let adapter = env.adapter(&[
        ("FAKE_OMP_VERSION_FAIL", "1"),
        ("FAKE_OMP_MODES", "noreport"),
    ]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    let AgentEvent::SessionMeta { details, .. } = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(details["omp_version"], Value::Null);
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_resume_passes_the_session_id_and_the_same_session_dir() {
    let env = Env::new();
    let first = echo_run(&env, &env.cfg()).await;
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "echo,noreport")]);
    let mut handle = adapter
        .resume("omp-session-9", "again", &env.cfg())
        .unwrap();
    let events = until_turn_completed(&mut handle).await;
    let AgentEvent::SessionMeta {
        adapter_session_id, ..
    } = &events[0]
    else {
        panic!("{events:?}");
    };
    assert_eq!(adapter_session_id, "omp-session-9");
    let resumed = echo_report(&events);
    let (a, b) = (argv_of(&first), argv_of(&resumed));
    assert_eq!(arg_after(&b, "--resume").as_deref(), Some("omp-session-9"));
    assert_eq!(arg_after(&a, "--resume"), None);
    assert_eq!(
        arg_after(&a, "--session-dir"),
        Some(env.state.join("sessions").to_string_lossy().into_owned())
    );
    assert_eq!(
        arg_after(&a, "--session-dir"),
        arg_after(&b, "--session-dir")
    );
    drain(&mut handle).await;
}

// ---------------------------------------------------------------------------
// Spawn arguments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_isolated_overlay_has_every_key_and_pins_denials_outside_a_worktree() {
    let env = Env::new();
    for sandboxed in [true, false] {
        let mut cfg = env.cfg();
        cfg.sandboxed = sandboxed;
        let report = echo_run(&env, &cfg).await;
        assert_eq!(report["overlay_mode"], "0o600");
        let overlay = overlay_of(&report);
        assert_eq!(
            overlay["disabledProviders"],
            json!([
                "native",
                "claude-md",
                "agents-md",
                "agents",
                "codex",
                "gemini",
                "opencode",
                "github",
                "cursor",
                "windsurf",
                "cline",
                "vscode",
                "mcp-json",
                "claude-plugins",
                "omp-plugins"
            ])
        );
        assert_eq!(
            overlay["disabledExtensions"],
            json!(["context-file:user:AGENTS.md", "context-file:user:CLAUDE.md"])
        );
        assert_eq!(
            overlay["skills"],
            json!({"enableClaudeUser": false, "enableCodexUser": false,
                   "enablePiUser": false, "enableAgentsUser": false})
        );
        assert_eq!(overlay["enabledProviders"], json!([]));
        assert_eq!(overlay["mcp"], json!({"enableProjectConfig": false}));
        assert_eq!(overlay["memory"], json!({"backend": "off"}));
        assert_eq!(overlay["memories"], json!({"enabled": false}));
        assert_eq!(overlay["advisor"], json!({"enabled": false}));
        assert_eq!(overlay["async"], json!({"enabled": false}));
        assert_eq!(
            overlay["bash"],
            json!({"autoBackground": {"enabled": false}})
        );
        assert_eq!(overlay["dev"], json!({"autoqa": false}));
        assert_eq!(overlay["telemetry"], json!({"otlpExportEnabled": false}));
        let expected = if sandboxed {
            json!({"report_outcome": "allow"})
        } else {
            json!({"report_outcome": "allow", "bash": "deny", "edit": "deny", "write": "deny"})
        };
        assert_eq!(overlay["tools"], json!({ "approval": expected }));
    }
}

#[tokio::test]
async fn the_inherit_overlay_has_exactly_three_keys_and_inherits_the_rest_of_the_setup() {
    let env = Env::new();
    for sandboxed in [true, false] {
        let mut cfg = env.cfg();
        cfg.isolation = Isolation::InheritOperatorConfig;
        cfg.sandboxed = sandboxed;
        cfg.report_outcomes = Vec::new();
        let report = echo_run(&env, &cfg).await;
        let overlay = overlay_of(&report);
        let mut keys: Vec<&String> = overlay.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["dev", "telemetry", "tools"]);
        assert_eq!(overlay["dev"], json!({"autoqa": false}));
        assert_eq!(overlay["telemetry"], json!({"otlpExportEnabled": false}));
        let approval = &overlay["tools"]["approval"];
        assert_eq!(
            approval.as_object().unwrap().len(),
            if sandboxed { 1 } else { 4 }
        );
        assert_eq!(approval["report_outcome"], "allow");
        let argv = argv_of(&report);
        for absent in [
            "--no-rules",
            "--skills",
            "--no-skills",
            "--system-prompt",
            "--append-system-prompt",
        ] {
            assert!(!argv.iter().any(|arg| arg == absent), "{absent}: {argv:?}");
        }
        assert_eq!(
            arg_after(&argv, "--approval-mode").as_deref(),
            Some(if sandboxed { "yolo" } else { "always-ask" })
        );
        assert!(arg_after(&argv, "--tools").is_some());
        assert_eq!(arg_after(&argv, "--thinking").as_deref(), Some("medium"));
    }
}

#[tokio::test]
async fn an_inherit_role_gets_its_system_prompt_and_the_report_instruction_only_when_it_has_them() {
    let env = Env::new();
    let mut cfg = env.cfg();
    cfg.isolation = Isolation::InheritOperatorConfig;
    cfg.system_prompt = Some("Be chatty".to_string());
    let argv = argv_of(&echo_run(&env, &cfg).await);
    assert_eq!(
        arg_after(&argv, "--system-prompt").as_deref(),
        Some("Be chatty\n")
    );
    assert_eq!(
        arg_after(&argv, "--append-system-prompt").unwrap(),
        with_trailing_newline(&report_instruction(&cfg.report_outcomes))
    );
}

#[tokio::test]
async fn approval_mode_follows_the_sandbox_and_is_never_write() {
    let env = Env::new();
    for (sandboxed, expected) in [(true, "yolo"), (false, "always-ask")] {
        for isolation in [Isolation::default(), Isolation::InheritOperatorConfig] {
            let mut cfg = env.cfg();
            cfg.sandboxed = sandboxed;
            cfg.isolation = isolation;
            let argv = argv_of(&echo_run(&env, &cfg).await);
            assert_eq!(
                arg_after(&argv, "--approval-mode").as_deref(),
                Some(expected)
            );
            assert_ne!(
                arg_after(&argv, "--approval-mode").as_deref(),
                Some("write")
            );
        }
    }
}

#[tokio::test]
async fn the_append_prompt_is_always_present_and_holds_the_repo_files_in_order() {
    let env = Env::new();
    // Nothing to say: the fixed sentence.
    let mut cfg = env.cfg();
    cfg.report_outcomes = Vec::new();
    let argv = argv_of(&echo_run(&env, &cfg).await);
    assert_eq!(
        arg_after(&argv, "--append-system-prompt").as_deref(),
        Some("No repository instruction files were found.\n")
    );

    std::fs::create_dir_all(env.repo.join(".omp")).unwrap();
    std::fs::write(env.repo.join("CLAUDE.md"), "claude rules\n").unwrap();
    std::fs::write(env.repo.join("AGENTS.md"), "agent rules").unwrap();
    std::fs::write(env.repo.join(".omp/AGENTS.md"), "omp agents\n").unwrap();
    std::fs::write(env.repo.join(".omp/RULES.md"), "omp rules\n").unwrap();
    std::fs::write(env.repo.join(".omp/mcp.json"), "{\"mcpServers\":{}}").unwrap();
    // With outcomes: files, then the report instruction.
    let cfg = env.cfg();
    let argv = argv_of(&echo_run(&env, &cfg).await);
    let append = arg_after(&argv, "--append-system-prompt").unwrap();
    assert!(append.ends_with('\n'));
    let positions: Vec<usize> = [
        "<instructions source=\"CLAUDE.md\">\nclaude rules\n</instructions>\n",
        "<instructions source=\"AGENTS.md\">\nagent rules\n</instructions>\n",
        "<instructions source=\".omp/AGENTS.md\">\nomp agents\n</instructions>\n",
        "<instructions source=\".omp/RULES.md\">\nomp rules\n</instructions>\n",
        &report_instruction(&cfg.report_outcomes),
    ]
    .iter()
    .map(|part| {
        append
            .find(part)
            .unwrap_or_else(|| panic!("{part} missing in {append}"))
    })
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{positions:?}"
    );
    assert!(!append.contains("mcpServers"));
}

#[tokio::test]
async fn a_repo_file_symlinked_outside_the_repo_is_skipped_with_a_warning() {
    let env = Env::new();
    let outside = env.dir.join("secret.md");
    std::fs::write(&outside, "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(&outside, env.repo.join("CLAUDE.md")).unwrap();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "echo,noreport")]);
    let mut handle = adapter.start("hi", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    let warn = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Error { message } if message.contains("CLAUDE.md") && message.contains("outside")))
        .unwrap_or_else(|| panic!("no warning in {events:?}"));
    let meta = events
        .iter()
        .position(|event| matches!(event, AgentEvent::SessionMeta { .. }))
        .unwrap();
    assert!(warn < meta, "the warning comes before SessionMeta");
    let argv = argv_of(&echo_report(&events));
    assert!(
        !arg_after(&argv, "--append-system-prompt")
            .unwrap()
            .contains("TOP SECRET")
    );
    drain(&mut handle).await;
}

#[tokio::test]
async fn system_prompt_flags_skills_thinking_tools_and_the_profile_rule() {
    let env = Env::new();
    // Default system prompt, no skills, default thinking, full toolset.
    let mut cfg = env.cfg();
    let argv = argv_of(&echo_run(&env, &cfg).await);
    assert_eq!(
        arg_after(&argv, "--system-prompt").as_deref(),
        Some(DEFAULT_SYSTEM_PROMPT)
    );
    assert!(argv.contains(&"--no-skills".to_string()));
    assert!(argv.contains(&"--no-rules".to_string()));
    assert!(!argv.iter().any(|arg| arg.starts_with("--profile")));
    for flag in [
        "--mode",
        "--no-ui",
        "--no-lsp",
        "--no-title",
        "--config",
        "--session-dir",
    ] {
        assert!(argv.contains(&flag.to_string()), "{flag}");
    }
    assert_eq!(arg_after(&argv, "--mode").as_deref(), Some("rpc"));
    assert_eq!(arg_after(&argv, "--thinking").as_deref(), Some("medium"));
    assert_eq!(
        arg_after(&argv, "--model").as_deref(),
        Some("openai-codex/gpt-5.6-terra")
    );
    assert_eq!(
        arg_after(&argv, "--tools").as_deref(),
        Some("read,bash,edit,write,glob,grep,todo")
    );

    // The role's system prompt gets a trailing newline; skills are joined.
    cfg.system_prompt = Some("Role text".to_string());
    cfg.isolation = Isolation::Isolated {
        skills: vec!["a".to_string(), "b".to_string()],
        memory: false,
    };
    let argv = argv_of(&echo_run(&env, &cfg).await);
    assert_eq!(
        arg_after(&argv, "--system-prompt").as_deref(),
        Some("Role text\n")
    );
    assert_eq!(arg_after(&argv, "--skills").as_deref(), Some("a,b"));
    assert!(!argv.contains(&"--no-skills".to_string()));

    // A level suffix wins over --thinking; no model still gets medium.
    cfg.model = Some("openai-codex/gpt-5.6-terra:high".to_string());
    let argv = argv_of(&echo_run(&env, &cfg).await);
    assert!(!argv.contains(&"--thinking".to_string()));
    assert_eq!(
        arg_after(&argv, "--model").as_deref(),
        Some("openai-codex/gpt-5.6-terra:high")
    );
    cfg.model = None;
    let argv = argv_of(&echo_run(&env, &cfg).await);
    assert_eq!(arg_after(&argv, "--thinking").as_deref(), Some("medium"));
    assert!(!argv.contains(&"--model".to_string()));
}

#[test]
fn the_tool_allowlist_drops_what_the_role_disallows_and_never_lists_report_outcome() {
    let tools = |disallowed: &[RoleTool]| allowed_tools(disallowed, false).join(",");
    assert_eq!(tools(&[]), "read,bash,edit,write,glob,grep,todo");
    assert_eq!(tools(&[RoleTool::Edit]), "read,bash,write,glob,grep,todo");
    assert_eq!(tools(&[RoleTool::Write]), "read,bash,edit,glob,grep,todo");
    assert_eq!(tools(&[RoleTool::NotebookEdit]), tools(&[]));
    assert!(!tools(&RoleTool::ALL).contains("report_outcome"));
    for suffix in ["off", "high", "auto"] {
        assert!(model_has_thinking_suffix(&format!("p/m:{suffix}")));
    }
    assert!(!model_has_thinking_suffix("p/m"));
    assert!(!model_has_thinking_suffix("p/m:latest"));
}

#[tokio::test]
async fn the_child_environment_is_scrubbed_and_the_otel_sdk_is_off() {
    let env = Env::new();
    let report = echo_run(&env, &env.cfg()).await;
    assert_eq!(report["env"]["OTEL_SDK_DISABLED"], "true");
    let env_obj = report["env"].as_object().unwrap();
    assert!(
        env_obj
            .keys()
            .filter(|key| key.starts_with("OTEL_"))
            .all(|key| key == "OTEL_SDK_DISABLED"),
        "{env_obj:?}"
    );
    for gone in [
        "OMP_PROFILE",
        "PI_PROFILE",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
    ] {
        assert!(!env_obj.contains_key(gone), "{gone}");
    }

    // What the scrub removes, checked on the command itself so the test
    // needn't touch this process's environment: every name it is told to
    // drop is recorded as removed, and the SDK is switched off.
    let mut command = Command::new("true");
    scrub_env(&mut command);
    let envs: HashMap<String, Option<String>> = command
        .as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for gone in [
        "OMP_PROFILE",
        "PI_PROFILE",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
    ] {
        assert_eq!(envs.get(gone), Some(&None), "{gone}");
    }
    assert_eq!(
        envs.get("OTEL_SDK_DISABLED"),
        Some(&Some("true".to_string()))
    );
}

#[test]
fn every_otel_variable_in_the_environment_is_removed() {
    let mut command = Command::new("true");
    let names = ["OTEL_CHOCO_SCRUB_TEST", "PATH"].map(std::ffi::OsString::from);
    scrub_env_from(&mut command, names.into_iter());
    let get = |name: &str| {
        command
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.map(|v| v.to_owned()))
    };
    assert_eq!(get("OTEL_CHOCO_SCRUB_TEST"), Some(None));
    assert_eq!(get("PATH"), None);
    // These beat the overlay, so the child must never inherit them.
    for name in [
        "PI_AUTO_QA",
        "PI_AUTO_QA_PUSH",
        "PI_AUTO_QA_PUSH_URL",
        "PI_AUTO_QA_PUSH_TOKEN",
        "PI_CONFIG_FILES",
        "OMP_PROFILE",
    ] {
        assert_eq!(get(name), Some(None), "{name} must be removed");
    }
    assert_eq!(get("OTEL_SDK_DISABLED"), Some(Some("true".into())));
}

// ---------------------------------------------------------------------------
// Overlay lifetime and the fail-closed paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_overlay_is_removed_when_the_handle_drops() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    until_turn_completed(&mut handle).await;
    let overlays = env.state.join("overlays");
    let count = || std::fs::read_dir(&overlays).unwrap().count();
    assert_eq!(count(), 1, "the overlay exists while the session lives");
    drain(&mut handle).await;
    handle.wait().await.unwrap();
    assert_eq!(count(), 1, "and survives the process exiting");
    drop(handle);
    assert_eq!(count(), 0);
}

#[tokio::test]
async fn an_overlay_that_cannot_be_written_fails_the_spawn_and_starts_no_process() {
    let env = Env::new();
    // The state dir is a file, so neither directory can be created.
    std::fs::write(&env.state, "not a directory").unwrap();
    let marker = env.dir.join("omp-was-run");
    let binary = wrapper_for(&env.dir, "fake_omp.py", &[], Some(&marker));
    let adapter = OmpAdapter::with_binary(binary, &env.state);
    let err = adapter
        .start("go", &env.cfg())
        .err()
        .expect("spawn must fail");
    assert!(matches!(err, AdapterError::Spawn(_)), "{err:?}");
    assert!(!marker.exists(), "omp must not have been started");
}

#[tokio::test]
async fn a_failed_process_spawn_removes_the_overlay_it_wrote() {
    let env = Env::new();
    let adapter =
        OmpAdapter::with_binary(env.dir.join("no-such-binary").to_string_lossy(), &env.state);
    let err = adapter
        .start("go", &env.cfg())
        .err()
        .expect("spawn must fail");
    assert!(matches!(err, AdapterError::Spawn(_)));
    assert_eq!(
        std::fs::read_dir(env.state.join("overlays"))
            .unwrap()
            .count(),
        0
    );
}

// ---------------------------------------------------------------------------
// omp's state folder, the Claude MCP pins and the home-test guard (#308)
// ---------------------------------------------------------------------------

/// Removes a folder when dropped, even while unwinding from a panic.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        // A test guard: the folder may never have been created.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn chat_cfg(env: &Env) -> RoleConfig {
    RoleConfig {
        isolation: Isolation::InheritOperatorConfig,
        ..env.cfg()
    }
}

async fn echo_with(env: &Env, adapter: &OmpAdapter, cfg: &RoleConfig, resume: bool) -> Value {
    let mut handle = if resume {
        adapter.resume("s-1", "hi", cfg).unwrap()
    } else {
        adapter.start("hi", cfg).unwrap()
    };
    let events = until_turn_completed(&mut handle).await;
    let _ = env;
    echo_report(&events)
}

#[tokio::test]
async fn every_omp_process_gets_a_prepared_state_folder_as_xdg_state_home() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "echo,noreport")]);
    let expected = env.state.join("xdg-state").to_string_lossy().into_owned();
    for (cfg, resume) in [
        (env.cfg(), false),
        (env.cfg(), true),
        (chat_cfg(&env), false),
    ] {
        let report = echo_with(&env, &adapter, &cfg, resume).await;
        assert_eq!(report["env"]["XDG_STATE_HOME"], expected.as_str());
        assert_eq!(report["state_omp_dir_at_start"], true);
    }
    use std::os::unix::fs::MetadataExt;
    for dir in [env.state.join("xdg-state"), env.state.join("xdg-state/omp")] {
        assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
    }
}

#[tokio::test]
async fn a_value_in_the_daemons_environment_does_not_win_over_the_redirect() {
    let env = Env::new();
    let adapter = env
        .adapter(&[("FAKE_OMP_MODES", "echo,noreport")])
        .with_parent_env(&[("XDG_STATE_HOME", "/elsewhere/xdg")]);
    let report = echo_with(&env, &adapter, &env.cfg(), false).await;
    assert_eq!(
        report["env"]["XDG_STATE_HOME"],
        env.state.join("xdg-state").to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn omp_version_runs_in_the_turns_environment_and_not_at_all_without_the_folder() {
    let env = Env::new();
    let log = env.dir.join("version.log");
    let binary = wrapper(
        &env.dir,
        &[("FAKE_OMP_VERSION_LOG", &log.to_string_lossy())],
    );
    let launch = OmpAdapter::with_binary(&binary, &env.state).launch(&env.cfg());
    assert_eq!(
        omp_version(&binary, &launch).await.as_deref(),
        Some("omp/fake-1.0")
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("{}\nyes\n", env.state.join("xdg-state").display())
    );

    std::fs::remove_file(&log).unwrap();
    std::fs::remove_dir_all(env.state.join("xdg-state")).unwrap();
    std::fs::write(env.state.join("xdg-state"), "a file").unwrap();
    assert_eq!(omp_version(&binary, &launch).await, None);
    assert!(!log.exists(), "omp --version must not have run");
}

#[tokio::test]
async fn a_state_folder_that_cannot_be_prepared_refuses_the_spawn_before_the_overlay() {
    let env = Env::new();
    std::fs::create_dir_all(&env.state).unwrap();
    std::fs::write(env.state.join("xdg-state"), "a file").unwrap();
    let launch_log = env.dir.join("launch.log");
    let adapter = env.adapter(&[("FAKE_OMP_LAUNCH_LOG", &launch_log.to_string_lossy())]);
    let err = adapter.start("go", &env.cfg()).err().expect("must fail");
    let AdapterError::Spawn(err) = err else {
        panic!("{err:?}")
    };
    let text = err.to_string();
    assert!(text.contains("role 'coder'"), "{text}");
    assert!(
        text.contains(&env.state.join("xdg-state").to_string_lossy().to_string()),
        "{text}"
    );
    assert!(
        std::fs::read_to_string(&launch_log)
            .unwrap_or_default()
            .is_empty()
    );
    let overlays = env.state.join("overlays");
    assert!(!overlays.exists() || std::fs::read_dir(overlays).unwrap().count() == 0);
}

#[test]
fn a_client_file_belongs_to_exactly_one_pid() {
    assert!(is_client_file_of("1234-ab.json", 1234));
    assert!(!is_client_file_of("12345-ab.json", 1234));
    assert!(!is_client_file_of("1234.json", 1234));
    assert!(!is_client_file_of("1234-ab.json.tmp", 1234));
}

/// The `clients/` files in `<state>/xdg-state/omp/run/daemons/*/clients/`.
fn client_files(env: &Env) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(daemons) = std::fs::read_dir(env.state.join("xdg-state/omp/run/daemons")) else {
        return found;
    };
    for daemon in daemons.flatten() {
        if let Ok(files) = std::fs::read_dir(daemon.path().join("clients")) {
            found.extend(
                files
                    .flatten()
                    .map(|f| f.file_name().to_string_lossy().into_owned()),
            );
        }
    }
    found.sort();
    found
}

async fn wait_for_client_file(env: &Env, pid: u32) {
    let prefix = format!("{pid}-");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !client_files(env)
        .iter()
        .any(|name| name.starts_with(&prefix))
    {
        assert!(Instant::now() < deadline, "the fake wrote no client file");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_finished_turn_removes_its_own_client_file_and_no_other() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "client_file,noreport")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let pid = handle.pid().expect("pid");
    wait_for_client_file(&env, pid).await;
    let daemons = env.state.join("xdg-state/omp/run/daemons");
    let longer = daemons
        .join("e2f034ac33e93976/clients")
        .join(format!("{pid}5-aaaa.json"));
    let other_dir = daemons.join("0123456789abcdef/clients");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other = other_dir.join(format!("{}-bbbb.json", pid + 1));
    std::fs::write(&longer, "{}").unwrap();
    std::fs::write(&other, "{}").unwrap();
    until_turn_completed(&mut handle).await;
    drain(&mut handle).await;
    handle.wait().await.unwrap();
    drop(handle);
    let prefix = format!("{pid}-");
    assert!(
        !client_files(&env).iter().any(|n| n.starts_with(&prefix)),
        "{:?}",
        client_files(&env)
    );
    assert!(longer.exists() && other.exists());
    assert!(daemons.join("e2f034ac33e93976/clients").is_dir());
    assert!(other_dir.is_dir());
}

#[tokio::test]
async fn an_abandoned_turn_removes_its_client_file() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "client_file,no_ready")]);
    let handle = adapter.start("go", &env.cfg()).unwrap();
    let pid = handle.pid().expect("pid");
    wait_for_client_file(&env, pid).await;
    drop(handle);
    assert_eq!(client_files(&env), Vec::<String>::new());
}

#[test]
fn the_isolated_overlay_pins_enabled_providers_and_the_chat_overlay_does_not() {
    let env = Env::new();
    assert_eq!(overlay(&env.cfg(), None)["enabledProviders"], json!([]));
    let db = env.dir.join("mnemopi.db");
    assert_eq!(
        overlay(&env.memory_cfg("P"), Some(&db))["enabledProviders"],
        json!([])
    );
    assert!(
        overlay(&chat_cfg(&env), None)
            .get("enabledProviders")
            .is_none()
    );
}

#[tokio::test]
async fn claude_config_dir_is_removed_for_isolated_roles_and_kept_for_chat() {
    let env = Env::new();
    let dir = env.dir.join("claude").to_string_lossy().into_owned();
    let adapter = env
        .adapter(&[("FAKE_OMP_MODES", "echo,noreport")])
        .with_parent_env(&[("CLAUDE_CONFIG_DIR", &dir)]);
    let isolated = echo_with(&env, &adapter, &env.cfg(), false).await;
    assert!(isolated["env"].get("CLAUDE_CONFIG_DIR").is_none());
    let chat = echo_with(&env, &adapter, &chat_cfg(&env), false).await;
    assert_eq!(chat["env"]["CLAUDE_CONFIG_DIR"], dir.as_str());
}

#[test]
fn the_home_test_guard_removes_its_folder_when_the_test_panics() {
    let dir = tempdir("guard");
    std::fs::write(dir.join("file"), "x").unwrap();
    let guarded = dir.clone();
    let result = std::panic::catch_unwind(move || {
        let _guard = RemoveOnDrop(guarded);
        panic!("a failing test");
    });
    assert!(result.is_err());
    assert!(!dir.exists());
}

/// Starts the real omp as an isolated role under a fake `HOME` whose
/// `~/.claude.json` (or `claude_dir/.claude.json`) names an MCP server that
/// only touches a marker file; returns whether the marker appeared. No model
/// call: a dummy key lets omp reach `ready`.
async fn personal_mcp_server_started(
    enabled_all: bool,
    claude_config_dir: bool,
    parent_binary: &str,
) -> bool {
    let env = Env::new();
    let home = env.dir.join("home");
    let marker = env.dir.join("mcp-started");
    std::fs::create_dir_all(home.join(".omp/agent")).unwrap();
    let mut config = String::from("mcp:\n  startupTimeoutMs: 0\n");
    if enabled_all {
        config.push_str("enabledProviders: [\"*\"]\n");
    }
    std::fs::write(home.join(".omp/agent/config.yml"), config).unwrap();
    let servers = json!({"mcpServers": {"touch_marker": {
        "command": "sh",
        "args": ["-c", format!("touch '{}'; sleep 30", marker.display())]}}});
    let claude_dir = env.dir.join("claude-dir");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(home.join(".claude.json"), servers.to_string()).unwrap();
    std::fs::write(claude_dir.join(".claude.json"), servers.to_string()).unwrap();
    let home_s = home.to_string_lossy().into_owned();
    let claude_s = claude_dir.to_string_lossy().into_owned();
    let mut parent = vec![("HOME", home_s.as_str()), ("OPENAI_API_KEY", "sk-dummy")];
    if claude_config_dir {
        parent.push(("CLAUDE_CONFIG_DIR", claude_s.as_str()));
    }
    let mut cfg = env.cfg();
    cfg.report_outcomes = vec!["done".to_string()];
    let (mut command, overlay_path) =
        real_command_with_env(parent_binary, &env.state, &cfg, None, &parent);
    let mut child = command.spawn().expect("omp must be on PATH");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let ready = tokio::time::timeout(Duration::from_secs(60), read_lf_line(&mut lines))
        .await
        .expect("no ready in 60 s")
        .unwrap()
        .expect("omp exited");
    assert!(ready.contains("\"ready\""), "{ready}");
    rpc_request(&mut stdin, &mut lines, "s", json!({"type": "get_state"})).await;
    drop(stdin);
    let _ = child.kill().await;
    let _ = std::fs::remove_file(overlay_path);
    marker.exists()
}

/// Run with: `cargo test -p chocofactoryd --lib no_personal_mcp_server_starts -- --ignored`
#[tokio::test]
#[ignore = "needs a real omp on PATH (no model call is made)"]
async fn no_personal_mcp_server_starts_when_the_operator_enables_every_provider() {
    assert!(!personal_mcp_server_started(true, false, "omp").await);
}

#[tokio::test]
#[ignore = "needs a real omp on PATH (no model call is made)"]
async fn no_personal_mcp_server_starts_when_claude_config_dir_is_set() {
    assert!(!personal_mcp_server_started(false, true, "omp").await);
}

/// Every file under `root`: path -> (size, mtime).
fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    let mut found = std::collections::BTreeMap::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                todo.push(entry.path());
            } else {
                found.insert(entry.path(), (meta.len(), meta.modified().unwrap()));
            }
        }
    }
    found
}

/// The before/after check of `~/.omp` (#308): one real turn through the
/// adapter must leave nothing naming it in the operator's omp folder, except
/// inside `agent/agent.db` and `agent/models.db`.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_turn_leaves_nothing_in_the_operators_omp_folder -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real omp, its login, and makes one model call"]
async fn omp_turn_leaves_nothing_in_the_operators_omp_folder() {
    let omp_home = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".omp");
    let env = Env::new();
    let adapter = OmpAdapter::new(&env.state);
    let mut cfg = env.cfg();
    cfg.report_outcomes = vec!["done".to_string()];
    let before = snapshot(&omp_home);
    let mut handle = adapter
        .start(
            "Reply with the single word ok, then report your outcome as done.",
            &cfg,
        )
        .expect("omp must be on PATH");
    let pid = handle.pid().expect("pid");
    let events = until_turn_completed(&mut handle).await;
    let session_id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SessionMeta {
                adapter_session_id, ..
            } => Some(adapter_session_id.clone()),
            _ => None,
        })
        .expect("a SessionMeta");
    // Still alive: its client file is in choco's folder; check the pid
    // against ps once.
    let prefix = format!("{pid}-");
    // Recorded now, asserted after the leak check, so a leak into `~/.omp`
    // is reported with its paths first.
    let client_files_alive = client_files(&env);
    let ps = std::process::Command::new("ps")
        .args(["-o", "pid=,comm=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    println!("ps for pid {pid}: {}", String::from_utf8_lossy(&ps.stdout));
    drain(&mut handle).await;
    handle.wait().await.unwrap();
    drop(handle);
    let after = snapshot(&omp_home);

    let mut changed: Vec<_> = after
        .iter()
        .filter(|(path, stat)| before.get(*path) != Some(stat))
        .collect();
    changed.sort();
    println!("added or changed under {}:", omp_home.display());
    for (path, (size, mtime)) in &changed {
        println!("  {} ({size} bytes, {mtime:?})", path.display());
    }
    let needles = [
        session_id.clone(),
        env.state.join("sessions").to_string_lossy().into_owned(),
        env.repo.to_string_lossy().into_owned(),
        pid.to_string(),
    ];
    let mut leaks = Vec::new();
    for (path, (size, _)) in &changed {
        let name = path.to_string_lossy();
        if ["agent/agent.db", "agent/models.db"]
            .iter()
            .any(|db| name.contains(db))
        {
            continue;
        }
        let content = if *size <= 64 * 1024 {
            String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
        } else {
            String::new()
        };
        if needles
            .iter()
            .any(|n| name.contains(n.as_str()) || content.contains(n.as_str()))
        {
            leaks.push(path.display().to_string());
        }
    }
    let xdg = env.state.join("xdg-state/omp");
    println!("under {}:", xdg.display());
    for (path, (size, mtime)) in snapshot(&xdg) {
        println!("  {} ({size} bytes, {mtime:?})", path.display());
    }
    for var in [
        "WEZTERM_PANE",
        "TMUX_PANE",
        "KITTY_WINDOW_ID",
        "TERM_SESSION_ID",
        "ZELLIJ_PANE_ID",
        "CMUX_SURFACE_ID",
    ] {
        println!("{var} set: {}", std::env::var_os(var).is_some());
    }
    assert!(
        leaks.is_empty(),
        "files in ~/.omp naming this turn: {leaks:?}"
    );
    // Positive control: omp did write its state, into choco's folder.
    assert!(
        client_files_alive.iter().any(|n| n.starts_with(&prefix)),
        "{client_files_alive:?}"
    );
    assert!(
        xdg.join(format!("run/session-owners/{session_id}.lock"))
            .exists()
    );
    assert!(!client_files(&env).iter().any(|n| n.starts_with(&prefix)));
    // `omp --version` through the adapter's builder writes no file.
    let sizes_before = snapshot(&xdg).len();
    let launch = adapter.launch(&cfg);
    assert!(omp_version("omp", &launch).await.is_some());
    assert_eq!(snapshot(&xdg).len(), sizes_before);
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

fn close(a: Option<f64>, b: f64) {
    assert!((a.unwrap() - b).abs() < 1e-9, "{a:?} vs {b}");
}

#[tokio::test]
async fn a_turns_usage_is_the_statistics_delta_with_per_model_figures() {
    let env = Env::new();
    let adapter = env.adapter(&[]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    // Three assistant messages (the read call, the report call, the reply).
    assert_eq!(usage.model_turns, Some(3));
    assert_eq!(usage.counting, UsageCounting::PerTurn);
    assert_eq!(usage.billing, BillingMode::Subscription);
    assert_eq!(
        usage.tokens,
        TokenCounts {
            input: Some(300),
            output: Some(60),
            cache_read: Some(90),
            cache_write: Some(15),
        }
    );
    close(usage.cost_usd, 0.0369);
    let models = usage.models.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "openai-codex/gpt-5.6-terra");
    assert_eq!(models[0].tokens, usage.tokens);
    close(models[0].cost_usd, 0.0369);
    assert!(usage.wall_time_ms.is_some());
    drain(&mut handle).await;
}

#[tokio::test]
async fn an_empty_or_absent_provider_still_gives_one_by_model_entry() {
    for (env_vars, key) in [
        (vec![("FAKE_OMP_PROVIDER", "")], "unknown/gpt-5.6-terra"),
        (vec![("FAKE_OMP_NO_PROVIDER", "1")], "unknown/gpt-5.6-terra"),
    ] {
        let env = Env::new();
        let adapter = env.adapter(&env_vars);
        let mut handle = adapter.start("go", &env.cfg()).unwrap();
        let usage = usage_of(&until_turn_completed(&mut handle).await);
        let models = usage.models.unwrap();
        assert_eq!(models.len(), 1, "{models:?}");
        assert_eq!(models[0].model, key);
        assert_eq!(models[0].tokens, usage.tokens);
        assert_eq!(usage.tokens.input, Some(300));
        drain(&mut handle).await;
    }
}

#[tokio::test]
async fn side_call_tokens_are_counted_and_a_resumed_first_turn_is_a_delta() {
    let env = Env::new();
    let adapter = env.adapter(&[
        ("FAKE_OMP_MODES", "noreport,side_calls"),
        ("FAKE_OMP_SIDE_TOKENS", "1000"),
        ("FAKE_OMP_START_TOKENS", "5000"),
    ]);
    let mut handle = adapter.resume("omp-session-3", "go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    // 200 from the two messages + 1000 nobody reported; the 5000 the
    // resumed session already had is not this turn's.
    assert_eq!(usage.tokens.input, Some(1200));
    let models = usage.models.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "openai-codex/gpt-5.6-terra");
    assert_eq!(models[0].tokens.input, Some(1200));
    drain(&mut handle).await;
}

fn full_counts() -> TokenCounts {
    TokenCounts {
        input: Some(300),
        output: Some(60),
        cache_read: Some(90),
        cache_write: Some(15),
    }
}

#[tokio::test]
async fn a_response_omp_discards_does_not_inflate_the_by_model_figures() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "discarded")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(usage.model_turns, Some(4));
    assert_eq!(usage.tokens, full_counts());
    close(usage.cost_usd, 0.0369);
    let models = usage.models.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "openai-codex/gpt-5.6-terra");
    assert_eq!(models[0].tokens, usage.tokens);
    close(models[0].cost_usd, 0.0369);
    drain(&mut handle).await;
}

#[tokio::test]
async fn two_models_split_the_delta_and_add_up() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,second_model")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(usage.model_turns, Some(3));
    assert_eq!(usage.tokens, full_counts());
    close(usage.cost_usd, 0.0369);
    let models = usage.models.unwrap();
    assert_eq!(models.len(), 2);
    let other = models
        .iter()
        .find(|m| m.model == "other-provider/other-model")
        .unwrap();
    let main = models
        .iter()
        .find(|m| m.model == "openai-codex/gpt-5.6-terra")
        .unwrap();
    assert_eq!(
        other.tokens,
        TokenCounts {
            input: Some(100),
            output: Some(20),
            cache_read: Some(30),
            cache_write: Some(5)
        }
    );
    close(other.cost_usd, 0.0123);
    assert_eq!(
        main.tokens,
        TokenCounts {
            input: Some(200),
            output: Some(40),
            cache_read: Some(60),
            cache_write: Some(10)
        }
    );
    close(main.cost_usd, 0.0246);
    let sum: f64 = models.iter().filter_map(|m| m.cost_usd).sum();
    close(Some(sum), 0.0369);
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_discarded_response_from_the_second_model_leaves_the_main_the_remainder() {
    let env = Env::new();
    let adapter = env.adapter(&[
        ("FAKE_OMP_MODES", "noreport,second_model,discarded"),
        ("FAKE_OMP_DISCARD_SECOND", "1"),
    ]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(usage.model_turns, Some(4));
    let models = usage.models.unwrap();
    let other = models
        .iter()
        .find(|m| m.model == "other-provider/other-model")
        .unwrap();
    let main = models
        .iter()
        .find(|m| m.model == "openai-codex/gpt-5.6-terra")
        .unwrap();
    assert_eq!(other.tokens.input, Some(200));
    assert_eq!(main.tokens.input, Some(100));
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_turn_with_no_messages_still_gives_the_main_model_its_figures() {
    let env = Env::new();
    let adapter = env.adapter(&[
        ("FAKE_OMP_MODES", "no_messages"),
        ("FAKE_OMP_SIDE_TOKENS", "1000"),
    ]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
    let usage = usage_of(&events);
    assert_eq!(usage.model_turns, Some(0));
    assert_eq!(usage.tokens.input, Some(1000));
    let models = usage.models.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model, "openai-codex/gpt-5.6-terra");
    assert_eq!(models[0].tokens.input, Some(1000));
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_missing_baseline_gives_no_data_for_the_first_turn() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_STATS_SEQ", "error,ok")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
    let usage = usage_of(&events);
    assert_eq!(usage.tokens, TokenCounts::default());
    assert_eq!(usage.cost_usd, None);
    assert!(usage.models.is_none());
    drain(&mut handle).await;
}

#[tokio::test]
async fn no_emitted_event_names_report_outcome_unqualified() {
    let env = Env::new();
    let adapter = env.adapter(&[]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let mut events = until_turn_completed(&mut handle).await;
    events.extend(drain(&mut handle).await);
    let qualified = qualified_report_outcome_tool_name();
    for event in &events {
        if let AgentEvent::SessionMeta { details, .. } = event {
            assert!(
                !details["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t == "report_outcome")
            );
        }
        if let AgentEvent::ToolCall { tool, .. } | AgentEvent::ToolResult { tool, .. } = event {
            assert_ne!(tool, "report_outcome");
        }
        assert!(
            !format!("{event:?}").contains("\"report_outcome\""),
            "{event:?}"
        );
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { tool, .. } if *tool == qualified))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { tool, .. } if *tool == qualified))
    );
}

#[tokio::test]
async fn an_unpriced_model_has_no_cost() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unpriced")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(usage.cost_usd, None);
    let models = usage.models.unwrap();
    assert_eq!(models[0].cost_usd, None);
    assert_eq!(models[0].tokens.input, Some(200));
    assert_eq!(usage.tokens.input, Some(200));
    drain(&mut handle).await;
}

#[tokio::test]
async fn a_bad_statistics_reading_gives_unknown_fields_and_the_turn_still_completes() {
    for mode in ["error", "garbled", "silent"] {
        let env = Env::new();
        // The baseline reading is fine; the turn's reading fails.
        let adapter = env.adapter(&[("FAKE_OMP_STATS_SEQ", &format!("ok,{mode}"))]);
        let mut handle = adapter.start("go", &env.cfg()).unwrap();
        let started = std::time::Instant::now();
        let events = until_turn_completed(&mut handle).await;
        if mode == "silent" {
            assert!(
                started.elapsed() >= Duration::from_secs(5),
                "waited for the timeout"
            );
        }
        let usage = usage_of(&events);
        assert_eq!(usage.tokens, TokenCounts::default(), "{mode}");
        assert_eq!(usage.cost_usd, None, "{mode}");
        // The report still happened and the turn is clean.
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                is_error: false,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolResult { tool, .. } if tool.contains("report_outcome"))));
        // Model turns come from the messages; a failed reading is no data.
        assert_eq!(usage.model_turns, Some(3));
        assert!(usage.models.is_none(), "{mode}");
        drain(&mut handle).await;
    }
}

#[tokio::test]
async fn the_turn_after_a_failed_reading_is_unknown_too_then_recovers() {
    let env = Env::new();
    // baseline ok, turn 1 fails, turn 2 ok (becomes the baseline), turn 3 ok.
    let adapter = env.adapter(&[
        ("FAKE_OMP_MODES", "noreport"),
        ("FAKE_OMP_STATS_SEQ", "ok,error,ok,ok"),
    ]);
    let mut handle = adapter.start("one", &env.cfg()).unwrap();
    let first = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(first.tokens.input, None);
    handle.send("two").unwrap();
    let second = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(second.tokens, TokenCounts::default());
    assert_eq!(second.cost_usd, None);
    handle.send("three").unwrap();
    let third = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(third.tokens.input, Some(200));
    close(third.cost_usd, 0.0246);
    drain(&mut handle).await;
}

#[test]
fn billing_follows_the_provider_and_the_environment() {
    let with = |vars: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    let none = with(&[]);
    for provider in [
        "openai-codex",
        "github-copilot",
        "cursor",
        "factory-droid",
        "google-gemini-cli",
        "google-antigravity",
    ] {
        assert_eq!(
            billing_for(provider, &none),
            BillingMode::Subscription,
            "{provider}"
        );
    }
    assert_eq!(
        billing_for("anthropic", &with(&[("ANTHROPIC_API_KEY", "k")])),
        BillingMode::ApiKey
    );
    assert_eq!(
        billing_for(
            "anthropic",
            &with(&[("ANTHROPIC_API_KEY", "k"), ("ANTHROPIC_OAUTH_TOKEN", "t")])
        ),
        BillingMode::Subscription
    );
    // An empty value is as good as unset.
    assert_eq!(
        billing_for(
            "anthropic",
            &with(&[("ANTHROPIC_OAUTH_TOKEN", ""), ("ANTHROPIC_API_KEY", "k")])
        ),
        BillingMode::ApiKey
    );
    assert_eq!(billing_for("anthropic", &none), BillingMode::Unknown);
    for (provider, key) in [
        ("openai", "OPENAI_API_KEY"),
        ("google", "GEMINI_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
        ("xai", "XAI_API_KEY"),
        ("groq", "GROQ_API_KEY"),
        ("mistral", "MISTRAL_API_KEY"),
        ("azure", "AZURE_OPENAI_API_KEY"),
    ] {
        let vars: &'static [(&'static str, &'static str)] = Box::leak(Box::new([(key, "k")]));
        assert_eq!(
            billing_for(provider, &with(vars)),
            BillingMode::ApiKey,
            "{provider}"
        );
        assert_eq!(
            billing_for(provider, &none),
            BillingMode::Unknown,
            "{provider}"
        );
    }
    assert_eq!(
        billing_for("some-new-provider", &with(&[("OPENAI_API_KEY", "k")])),
        BillingMode::Unknown
    );
}

// ---------------------------------------------------------------------------
// The memory / skill rule
// ---------------------------------------------------------------------------

#[test]
fn validate_role_accepts_memory_and_rejects_pattern_skill_names() {
    let adapter = OmpAdapter::new("/nonexistent");
    let isolated = |skills: &[&str], memory: bool| Isolation::Isolated {
        skills: skills.iter().map(|s| s.to_string()).collect(),
        memory,
    };
    assert!(
        adapter
            .validate_role("coder", &isolated(&["plain-skill"], false))
            .is_ok()
    );
    assert!(
        adapter
            .validate_role("chat", &Isolation::InheritOperatorConfig)
            .is_ok()
    );
    assert!(adapter.validate_role("coder", &isolated(&[], true)).is_ok());
    for bad in ["a,b", "a*", "a?", "[a]", "a{b}", "a}"] {
        assert_eq!(
            adapter
                .validate_role("coder", &isolated(&[bad], false))
                .unwrap_err(),
            format!(
                "role 'coder' lists skill '{bad}', which omp would read as a pattern; skill \
                 names for omp roles can't contain , * ? [ ] {{ }}"
            ),
            "{bad}"
        );
    }
}

#[test]
fn the_default_validate_role_accepts_everything() {
    let claude = ClaudeAdapter::with_binary("claude");
    assert!(
        claude
            .validate_role(
                "coder",
                &Isolation::Isolated {
                    skills: vec![],
                    memory: true
                }
            )
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// memory: true (#222)
// ---------------------------------------------------------------------------

fn memory_db_of(env: &Env, project: &str) -> PathBuf {
    env.state.join("memory").join(project).join("mnemopi.db")
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_memory_overlay_differs_from_the_plain_one_by_exactly_the_memory_block() {
    let env = Env::new();
    let db = memory_db_of(&env, "P");
    let plain = overlay(&env.cfg(), None);
    let memory = overlay(&env.memory_cfg("P"), Some(&db));

    let mut expected = plain.clone();
    expected["memory"] = json!({"backend": "mnemopi"});
    expected["mnemopi"] = json!({
        "dbPath": db.to_string_lossy(),
        "bank": "choco",
        "scoping": "global",
        "noEmbeddings": true,
        "llmMode": "none",
        "autoRecall": true,
        "autoRetain": true,
    });
    // No approval pin: omp runs both memory tools under `always-ask`.
    assert!(memory["tools"]["approval"].get("recall").is_none());
    assert_eq!(memory, expected);
    assert_eq!(memory["memories"]["enabled"], false);
    // The plain overlay is untouched.
    assert_eq!(plain["memory"], json!({"backend": "off"}));
    assert!(plain.get("mnemopi").is_none());
}

#[test]
fn only_memory_roles_get_recall_and_retain() {
    let mut cfg = Env::new().memory_cfg("P");
    let tools = |cfg: &RoleConfig| {
        let args = build_args(cfg, Path::new("/o.yml"), Path::new("/s"), None, None);
        let at = args.iter().position(|arg| arg == "--tools").unwrap();
        args[at + 1].clone()
    };
    assert_eq!(
        tools(&cfg),
        "read,bash,edit,write,glob,grep,todo,recall,retain"
    );
    let plain = Env::new().cfg();
    assert_eq!(tools(&plain), "read,bash,edit,write,glob,grep,todo");
    cfg.disallowed_tools = vec![RoleTool::Edit, RoleTool::Write];
    assert_eq!(tools(&cfg), "read,bash,glob,grep,todo,recall,retain");
}

#[tokio::test]
async fn a_memory_start_and_resume_create_an_owner_only_store_and_others_create_nothing() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport")]);
    let mut handle = adapter.start("hi", &env.memory_cfg("P")).unwrap();
    until_turn_completed(&mut handle).await;
    drain(&mut handle).await;
    let memory = env.state.join("memory");
    assert_eq!(mode_of(&memory), 0o700);
    assert_eq!(mode_of(&memory.join("P")), 0o700);

    // A resume tightens a folder that was made too open.
    for dir in [&memory, &memory.join("P")] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut handle = adapter.resume("s1", "hi", &env.memory_cfg("P")).unwrap();
    until_turn_completed(&mut handle).await;
    drain(&mut handle).await;
    assert_eq!(mode_of(&memory), 0o700);
    assert_eq!(mode_of(&memory.join("P")), 0o700);

    // memory: false and an inheriting (chat) role make no store.
    let other = Env::new();
    let adapter = other.adapter(&[("FAKE_OMP_MODES", "noreport")]);
    let mut inherit = other.cfg();
    inherit.isolation = Isolation::InheritOperatorConfig;
    for cfg in [other.cfg(), inherit] {
        let mut handle = adapter.start("hi", &cfg).unwrap();
        until_turn_completed(&mut handle).await;
        drain(&mut handle).await;
    }
    assert!(!other.state.join("memory").exists());
}

#[tokio::test]
async fn the_store_is_keyed_by_project_not_by_cwd() {
    let env = Env::new();
    let other_cwd = env.dir.join("worktree-2");
    std::fs::create_dir_all(&other_cwd).unwrap();
    let db_of = |report: &Value| {
        overlay_of(report)["mnemopi"]["dbPath"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let mut cfg = env.memory_cfg("P");
    let first = db_of(&echo_run(&env, &cfg).await);
    cfg.cwd = other_cwd;
    let second = db_of(&echo_run(&env, &cfg).await);
    let third = db_of(&echo_run(&env, &env.memory_cfg("Q")).await);
    assert_eq!(first, memory_db_of(&env, "P").to_string_lossy());
    assert_eq!(first, second);
    assert_ne!(first, third);
}

/// Events after a start (or resume) that fails: everything until the stream
/// ends, which only happens once the process has been killed.
async fn failed_session(env: &Env, stats: &str, resume: bool) -> (Vec<AgentEvent>, String) {
    failed_session_with(env, stats, resume, true).await
}

/// As `failed_session`; `drain_all` false stops at the first turn's end.
async fn failed_session_with(
    env: &Env,
    stats: &str,
    resume: bool,
    drain_all: bool,
) -> (Vec<AgentEvent>, String) {
    let log = env
        .dir
        .join(format!("prompts-{}.log", uuid::Uuid::new_v4()));
    let home = std::fs::canonicalize(&env.dir).unwrap();
    let adapter = env
        .adapter(&[
            ("HOME", &home.to_string_lossy()),
            ("FAKE_OMP_MODES", "noreport"),
            ("FAKE_OMP_MEMORY_STATS", stats),
            ("FAKE_OMP_PROMPT_LOG", &log.to_string_lossy()),
            (
                "FAKE_OMP_LAUNCH_LOG",
                &env.dir.join("launch.log").to_string_lossy(),
            ),
        ])
        .with_home(&home)
        .with_memory_check_timeout(Duration::from_secs(if stats == "hang" { 2 } else { 60 }));
    let cfg = env.memory_cfg("P");
    let mut handle = if resume {
        adapter.resume("s1", "do the work", &cfg).unwrap()
    } else {
        adapter.start("do the work", &cfg).unwrap()
    };
    let events = if drain_all {
        drain(&mut handle).await
    } else {
        let events = until_turn_completed(&mut handle).await;
        drain(&mut handle).await;
        events
    };
    (events, std::fs::read_to_string(&log).unwrap_or_default())
}

#[tokio::test]
async fn a_healthy_memory_check_runs_before_the_stage_prompt_without_follow_up() {
    for resume in [false, true] {
        let env = Env::new();
        let log = env.dir.join("prompts.log");
        let adapter = env.adapter(&[
            ("FAKE_OMP_MODES", "noreport"),
            ("FAKE_OMP_PROMPT_LOG", &log.to_string_lossy()),
        ]);
        let cfg = env.memory_cfg("P");
        let mut handle = if resume {
            adapter.resume("s1", "do the work", &cfg).unwrap()
        } else {
            adapter.start("do the work", &cfg).unwrap()
        };
        let events = until_turn_completed(&mut handle).await;
        drain(&mut handle).await;
        assert!(
            matches!(events[0], AgentEvent::SessionMeta { .. }),
            "{events:?}"
        );
        let lines: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines,
            vec![
                json!({"message": "/memory stats", "streamingBehavior": null}),
                json!({"message": "do the work", "streamingBehavior": null}),
            ],
            "resume={resume}"
        );
    }
}

/// The production adapter (no `with_home`) expands `~/` against the `HOME`
/// its omp children inherit. The store sits under the real `HOME`, which the
/// fake inherits too.
#[tokio::test]
async fn the_default_adapter_expands_home_relative_paths_against_its_own_home() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    assert_eq!(
        std::fs::canonicalize(&home).unwrap(),
        home,
        "this test needs a HOME without symlinks"
    );
    let mut env = Env::new();
    let under_home = home.join(format!(".choco-home-test-{}", uuid::Uuid::new_v4()));
    // Removed on every exit, including a failed assertion or the 30 s event
    // timeout (both panic). A run killed with SIGKILL can't clean up: accepted.
    let _cleanup = RemoveOnDrop(under_home.clone());
    env.state = under_home.join("state");
    let log = env.dir.join("prompts.log");
    let adapter = env.adapter(&[
        ("FAKE_OMP_MODES", "noreport"),
        ("FAKE_OMP_MEMORY_STATS", "home_relative"),
        ("FAKE_OMP_PROMPT_LOG", &log.to_string_lossy()),
    ]);
    let mut handle = adapter.start("do the work", &env.memory_cfg("P")).unwrap();
    let events = until_turn_completed(&mut handle).await;
    drain(&mut handle).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error { .. })),
        "{events:?}"
    );
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("do the work")
    );
}

#[tokio::test]
async fn a_store_omp_prints_as_home_relative_passes_the_check() {
    // Production keeps the store under $HOME, where real omp prints `~/…`.
    for resume in [false, true] {
        let env = Env::new();
        let (events, log) = failed_session_with(&env, "home_relative", resume, false).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error { .. })),
            "resume={resume}: {events:?}"
        );
        assert!(log.contains("do the work"), "resume={resume}: {log}");
    }
}

#[test]
fn a_home_relative_path_is_expanded_against_the_home_omp_ran_with() {
    let db = Path::new("/h/.config/choco/omp/memory/P/mnemopi.db");
    let frames = |path: &str| {
        vec![
            json!({"type": "command_output", "text": format!("| choco | `{path}` |\n")}),
            json!({"type": "response", "id": "m-1", "success": true,
                   "data": {"agentInvoked": false}}),
        ]
    };
    let verdict = |path: &str, home: Option<&str>| {
        memory_stats_verdict(&frames(path), "m-1", db, "coder", home.map(Path::new))
    };
    assert!(verdict("~/.config/choco/omp/memory/P/mnemopi.db", Some("/h")).is_ok());
    assert!(verdict("~/.config/choco/omp/memory/Q/mnemopi.db", Some("/h")).is_err());
    assert!(verdict("~/.omp/agent/memories/mnemopi/mnemopi.db", Some("/h")).is_err());
    // A `~/` path that expands somewhere else, or with no known HOME.
    assert!(verdict("~/.config/choco/omp/memory/P/mnemopi.db", Some("/other")).is_err());
    assert!(verdict("~/.config/choco/omp/memory/P/mnemopi.db", None).is_err());
}

#[test]
fn a_home_that_is_a_symlink_is_matched_through_its_canonical_path() {
    let tmp = tempdir("symlink");
    let real = std::fs::canonicalize(&tmp).unwrap();
    let link = real.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let db = real.join("x/mnemopi.db");
    let frames = vec![
        json!({"type": "command_output", "text": "| choco | `~/x/mnemopi.db` |\n"}),
        json!({"type": "response", "id": "m-1", "success": true,
               "data": {"agentInvoked": false}}),
    ];
    assert!(memory_stats_verdict(&frames, "m-1", &db, "coder", Some(&link)).is_ok());
    let other = real.join("y/mnemopi.db");
    assert!(memory_stats_verdict(&frames, "m-1", &other, "coder", Some(&link)).is_err());
}

#[tokio::test]
async fn a_memory_store_that_did_not_open_fails_the_turn_before_the_stage_prompt() {
    for resume in [false, true] {
        for stats in [
            "error",
            "other_path",
            "other_bank",
            "home_relative_other",
            "home_relative_default",
            "no_output",
            "agent",
            "exit",
            "hang",
        ] {
            let env = Env::new();
            let (events, log) = failed_session(&env, stats, resume).await;
            let db = std::fs::canonicalize(env.state.join("memory/P"))
                .unwrap()
                .join("mnemopi.db");
            let message = events
                .iter()
                .find_map(|event| match event {
                    AgentEvent::Error { message } => Some(message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{stats}: no Error in {events:?}"));
            assert!(message.contains("role 'coder'"), "{stats}: {message}");
            assert!(
                message.contains(&db.to_string_lossy().into_owned()),
                "{stats}: {message}"
            );
            if stats == "error" {
                assert!(message.contains("EACCES"), "{message}");
            }
            assert!(
                !log.contains("do the work"),
                "{stats} resume={resume}: the stage prompt was sent: {log}"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::SessionMeta { .. })),
                "{stats}: SessionMeta was emitted"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_roles_of_one_store_start_one_at_a_time() {
    // Each session hangs on /memory stats until its 2 s check deadline. Run
    // together on one store they must queue: the second's check starts only
    // after the first's ended.
    let env = Env::new();
    let started = std::time::Instant::now();
    let (a, b) = tokio::join!(
        failed_session(&env, "hang", false),
        failed_session(&env, "hang", false)
    );
    assert!(a.1.contains("/memory stats") && b.1.contains("/memory stats"));
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "the two checks overlapped: {:?}",
        started.elapsed()
    );
    // The second omp process must not even launch until the first's check
    // ended (about 2 s after its own launch), since omp opens the store as it
    // starts.
    let mut launches: Vec<f64> = std::fs::read_to_string(env.dir.join("launch.log"))
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    launches.sort_by(|x, y| x.partial_cmp(y).unwrap());
    assert_eq!(launches.len(), 2);
    assert!(
        launches[1] - launches[0] >= 1.9,
        "the second omp launched {:.2}s after the first",
        launches[1] - launches[0]
    );
}

#[test]
fn the_memory_verdict_needs_the_row_then_the_response_and_no_model_turn() {
    let db = Path::new("/state/memory/P/mnemopi.db");
    let output = |bank: &str, path: &str| {
        json!({"type": "command_output", "text": format!(
            "# Stats\n\n| Bank | Database |\n| --- | --- |\n| {bank} | `{path}` |\n")})
    };
    let response = |agent: bool| {
        json!({"type": "response", "id": "m-1", "command": "prompt",
               "success": true, "data": {"agentInvoked": agent}})
    };
    let verdict = |frames: &[Value]| memory_stats_verdict(frames, "m-1", db, "coder", None);
    assert!(
        verdict(&[
            output("choco", "/state/memory/P/mnemopi.db"),
            response(false)
        ])
        .is_ok()
    );
    // A mixed case: a wrong row before the right one still passes.
    assert!(
        verdict(&[
            output("choco-1a2b", "/state/memory/P/mnemopi.db"),
            output("choco", "/state/memory/P/mnemopi.db"),
            response(false)
        ])
        .is_ok()
    );
    for frames in [
        vec![response(false)],
        vec![
            response(false),
            output("choco", "/state/memory/P/mnemopi.db"),
        ],
        vec![output("choco", "/elsewhere/mnemopi.db"), response(false)],
        vec![
            output("choco-1a2b", "/state/memory/P/mnemopi.db"),
            response(false),
        ],
        vec![
            output("choco", "/state/memory/P/mnemopi.db"),
            response(true),
        ],
        vec![
            output("choco", "/state/memory/P/mnemopi.db"),
            json!({"type": "prompt_result", "id": "m-1"}),
        ],
        vec![output("choco", "/state/memory/P/mnemopi.db")],
    ] {
        let err = verdict(&frames).unwrap_err();
        assert!(
            err.contains("role 'coder'") && err.contains("mnemopi.db"),
            "{err}"
        );
    }
}

#[test]
fn a_store_path_that_is_a_file_fails_the_spawn_naming_the_role_and_path() {
    let env = Env::new();
    std::fs::create_dir_all(env.state.join("memory")).unwrap();
    std::fs::write(env.state.join("memory/P"), "not a folder").unwrap();
    let log = env.dir.join("prompts.log");
    let adapter = env.adapter(&[("FAKE_OMP_PROMPT_LOG", &log.to_string_lossy())]);
    let Err(AdapterError::Spawn(err)) = adapter.start("hi", &env.memory_cfg("P")) else {
        panic!("expected a spawn error");
    };
    let text = err.to_string();
    assert!(text.contains("role 'coder'"), "{text}");
    assert!(
        text.contains(&env.state.join("memory/P").to_string_lossy().into_owned()),
        "{text}"
    );
    assert!(!log.exists(), "omp must not have been started");
}

#[test]
fn a_memory_role_without_a_usable_project_id_fails_and_makes_no_store() {
    for project in [
        None,
        Some("../x"),
        Some("a/b"),
        Some(""),
        Some(".."),
        Some("/abs"),
    ] {
        let env = Env::new();
        let adapter = env.adapter(&[]);
        let mut cfg = env.memory_cfg("P");
        cfg.project_id = project.map(str::to_string);
        let Err(AdapterError::Spawn(err)) = adapter.start("hi", &cfg) else {
            panic!("{project:?}: expected a spawn error");
        };
        assert!(err.to_string().contains("role 'coder'"), "{err}");
        assert!(!env.state.join("memory").exists(), "{project:?}");
        assert!(!env.dir.join("x").exists());
    }
}

#[test]
fn removing_project_data_removes_only_that_projects_store() {
    let env = Env::new();
    let adapter = env.adapter(&[]);
    for project in ["P", "Q"] {
        let store = env.state.join("memory").join(project);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("mnemopi.db"), "x").unwrap();
    }
    adapter.remove_project_data("P").unwrap();
    assert!(!env.state.join("memory/P").exists());
    assert!(env.state.join("memory/Q/mnemopi.db").exists());
    // Nothing to remove, and a bad id.
    adapter.remove_project_data("P").unwrap();
    adapter.remove_project_data("never-had-one").unwrap();
    assert!(adapter.remove_project_data("../Q").is_err());
    assert!(adapter.remove_project_data("").is_err());
    assert!(env.state.join("memory/Q/mnemopi.db").exists());
}

// ---------------------------------------------------------------------------
// Opt-in tests against the real `omp` (ignored by default)
// ---------------------------------------------------------------------------

/// Everything `spawn` does before starting the process, for tests that talk
/// to the real omp themselves: the overlay on disk, the arguments, and the
/// scrubbed command.
fn real_command(
    binary: &str,
    state: &Path,
    cfg: &RoleConfig,
    memory_db: Option<&Path>,
) -> (Command, PathBuf) {
    real_command_with_env(binary, state, cfg, memory_db, &[])
}

/// `real_command` with `parent_env` standing in for the daemon's environment.
fn real_command_with_env(
    binary: &str,
    state: &Path,
    cfg: &RoleConfig,
    memory_db: Option<&Path>,
    parent_env: &[(&str, &str)],
) -> (Command, PathBuf) {
    let launch = OmpAdapter::with_binary(binary, state)
        .with_parent_env(parent_env)
        .launch(cfg);
    let (files, _warnings) = read_repo_instructions(&cfg.cwd, &INSTRUCTION_FILES);
    let append = match &cfg.isolation {
        Isolation::Isolated { .. } => Some(append_block(&render_instruction_files(&files), cfg)),
        Isolation::InheritOperatorConfig => None,
    };
    let overlay_dir = state.join("overlays");
    let session_dir = state.join("sessions");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut command = omp_command(binary, &launch).unwrap();
    let overlay_path = overlay_dir.join(format!("{}.yml", uuid::Uuid::new_v4()));
    write_private_file(
        &overlay_path,
        &serde_yaml::to_string(&overlay(cfg, memory_db)).unwrap(),
    )
    .unwrap();
    let args = build_args(cfg, &overlay_path, &session_dir, append.as_deref(), None);
    command
        .current_dir(&cfg.cwd)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    (command, overlay_path)
}

async fn rpc_request(
    stdin: &mut ChildStdin,
    lines: &mut BufReader<ChildStdout>,
    id: &str,
    body: Value,
) -> Value {
    let mut frame = body;
    frame["id"] = json!(id);
    stdin
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(60), read_lf_line(lines))
            .await
            .expect("omp didn't answer in 60 s")
            .unwrap()
            .expect("omp exited");
        let value: Value = serde_json::from_str(&line).unwrap();
        if value["type"] == "response" && value["id"] == id {
            assert_eq!(value["success"], true, "{value}");
            return value["data"].clone();
        }
    }
}

/// No model call. What the real omp loads under the adapter's own flags and
/// overlay: only the repo's four instruction files reach the system prompt
/// (nothing from a parent folder, a nested folder or `.omp/mcp.json`), and
/// the tool list is exactly the allowlist plus `report_outcome`.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_loads_only_the_repos_own_instructions -- --ignored`
#[tokio::test]
#[ignore = "needs a real omp on PATH (no model call is made)"]
async fn omp_loads_only_the_repos_own_instructions() {
    let env = Env::new();
    let parent = env.dir.join("parent");
    let repo = parent.join("repo");
    std::fs::create_dir_all(repo.join(".omp")).unwrap();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(parent.join("CLAUDE.md"), "PARENT_MARK\n").unwrap();
    std::fs::write(repo.join("CLAUDE.md"), "REPO_CLAUDE_MARK\n").unwrap();
    std::fs::write(repo.join("AGENTS.md"), "REPO_AGENTS_MARK\n").unwrap();
    std::fs::write(repo.join(".omp/AGENTS.md"), "REPO_OMP_AGENTS_MARK\n").unwrap();
    std::fs::write(repo.join(".omp/RULES.md"), "REPO_OMP_RULES_MARK\n").unwrap();
    std::fs::write(
        repo.join(".omp/mcp.json"),
        r#"{"mcpServers": {"mcp_mark_server": {"command": "true"}}}"#,
    )
    .unwrap();
    std::fs::write(repo.join("sub/CLAUDE.md"), "NESTED_MARK\n").unwrap();

    let mut cfg = env.cfg();
    cfg.cwd = repo;
    let (mut command, overlay_path) = real_command("omp", &env.state, &cfg, None);
    let mut child = command.spawn().expect("omp must be on PATH");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let tool = tool_definition(&StageReport {
        outcomes: cfg.report_outcomes.clone(),
        required_sections: Vec::new(),
    });
    rpc_request(
        &mut stdin,
        &mut lines,
        "t",
        json!({"type": "set_host_tools", "tools": [{
            "name": "report_outcome", "label": "Report outcome",
            "description": tool["description"], "parameters": tool["inputSchema"],
            "loadMode": "essential"}]}),
    )
    .await;
    let state = rpc_request(&mut stdin, &mut lines, "s", json!({"type": "get_state"})).await;
    drop(stdin);
    let _ = child.wait().await;
    let _ = std::fs::remove_file(overlay_path);

    let prompt = state["systemPrompt"].to_string();
    for present in [
        "REPO_CLAUDE_MARK",
        "REPO_AGENTS_MARK",
        "REPO_OMP_AGENTS_MARK",
        "REPO_OMP_RULES_MARK",
    ] {
        assert!(prompt.contains(present), "{present} missing");
    }
    for absent in ["PARENT_MARK", "NESTED_MARK", "mcp_mark_server"] {
        assert!(
            !prompt.contains(absent),
            "{absent} leaked into the system prompt"
        );
    }
    let mut tools: Vec<String> = state["dumpTools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    tools.sort();
    let mut expected: Vec<String> = BASE_TOOLS.iter().map(|tool| tool.to_string()).collect();
    expected.push("report_outcome".to_string());
    expected.sort();
    assert_eq!(tools, expected);
}

/// One real turn on `openai-codex/gpt-5.6-terra` with a one-line prompt,
/// through the adapter. Uses the operator's existing omp login and costs one
/// model prompt.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_runs_one_real_turn -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real omp, its login, and makes one model call"]
async fn omp_runs_one_real_turn() {
    let env = Env::new();
    let adapter = OmpAdapter::new(&env.state);
    let mut cfg = env.cfg();
    cfg.report_outcomes = vec!["done".to_string()];
    let mut handle = adapter
        .start(
            "Reply with the single word ok, then report your outcome as done.",
            &cfg,
        )
        .expect("omp must be on PATH");
    let events = until_turn_completed(&mut handle).await;
    println!("{events:#?}");
    let usage = usage_of(&events);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
    assert!(usage.models.is_some());
    assert_eq!(usage.billing, BillingMode::Subscription);
    drain(&mut handle).await;
}

/// Writes the exact spawn (arguments, overlay file, scrubbed environment)
/// the adapter would use for a role in `CHOCO_PROBE_CWD`, as JSON to
/// `CHOCO_PROBE_OUT`, so a throwaway probe script can drive the real omp the
/// way the adapter does. `CHOCO_PROBE_SANDBOXED=1` makes the role sandboxed.
///
/// Run with: `CHOCO_PROBE_CWD=<dir> CHOCO_PROBE_OUT=<file> cargo test -p chocofactoryd --lib dump_probe_spawn -- --ignored`
#[test]
#[ignore = "a helper for hand-run probes, not a test"]
fn dump_probe_spawn() {
    let cwd = PathBuf::from(std::env::var("CHOCO_PROBE_CWD").unwrap());
    let out = std::env::var("CHOCO_PROBE_OUT").unwrap();
    let state = PathBuf::from(&out).with_extension("state");
    let mut cfg = RoleConfig {
        role: "coder".to_string(),
        project_id: None,
        cwd,
        model: Some("openai-codex/gpt-5.6-terra".to_string()),
        system_prompt: None,
        sandboxed: std::env::var("CHOCO_PROBE_SANDBOXED").as_deref() == Ok("1"),
        report_outcomes: vec!["done".to_string()],
        report_sections: Vec::new(),
        isolation: Isolation::default(),
        disallowed_tools: Vec::new(),
    };
    cfg.report_outcomes = vec!["done".to_string()];
    let (command, overlay_path) = real_command("omp", &state, &cfg, None);
    let std_command = command.as_std();
    let args: Vec<String> = std_command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let env_changes: Vec<Value> = std_command
        .get_envs()
        .map(|(key, value)| json!([key.to_string_lossy(), value.map(|v| v.to_string_lossy())]))
        .collect();
    std::fs::write(
        out,
        json!({"args": args, "overlay": overlay_path, "env": env_changes}).to_string(),
    )
    .unwrap();
}

// -- memory: true against the real omp (#222) --

/// What the operator's own omp state looks like, for the "untouched" checks:
/// the legacy memories tree (path, size, mtime), whether the fastembed cache
/// and the mnemopi run folder exist, and `agent.db`'s table list (opened
/// read-only and immutable, never written).
#[derive(Debug, PartialEq)]
struct OmpHomeState {
    memories: Option<Vec<(PathBuf, u64, std::time::SystemTime)>>,
    fastembed: bool,
    run_mnemopi: bool,
    agent_db_tables: Vec<String>,
}

async fn omp_home_state() -> OmpHomeState {
    let home = PathBuf::from(std::env::var("HOME").unwrap()).join(".omp");
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            out.push((entry.path(), meta.len(), meta.modified().unwrap()));
            if meta.is_dir() {
                walk(&entry.path(), out);
            }
        }
    }
    let memories_dir = home.join("agent/memories");
    let memories = memories_dir.exists().then(|| {
        let mut files = Vec::new();
        walk(&memories_dir, &mut files);
        files.sort();
        files
    });
    let db = home.join("agent/agent.db");
    let mut agent_db_tables = Vec::new();
    if db.exists() {
        use sqlx::ConnectOptions;
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db)
            .read_only(true)
            .immutable(true)
            .connect()
            .await
            .expect("agent.db opens read-only");
        agent_db_tables =
            sqlx::query_scalar::<_, String>("SELECT name FROM sqlite_master ORDER BY name")
                .fetch_all(&mut conn)
                .await
                .unwrap();
    }
    OmpHomeState {
        memories,
        fastembed: home.join("cache/fastembed").exists(),
        run_mnemopi: home.join("run/mnemopi").exists(),
        agent_db_tables,
    }
}

async fn next_frame(lines: &mut BufReader<ChildStdout>) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(60), read_lf_line(lines))
        .await
        .expect("omp didn't answer in 60 s")
        .unwrap()
        .expect("omp exited");
    serde_json::from_str::<Value>(&line).unwrap()
}

/// Spawns the real omp with the adapter's own flags and overlay for a memory
/// role of project `P`, runs the same protocol-v2 negotiation the driver
/// runs, then `/memory stats` (no model call), and returns the verdict and
/// the stats table text.
async fn real_memory_stats(
    env: &Env,
    cfg: &RoleConfig,
    db: &Path,
    home: Option<&Path>,
) -> (Result<(), String>, String) {
    let (mut command, overlay_path) = real_command("omp", &env.state, cfg, Some(db));
    if let Some(home) = home {
        command.env("HOME", home);
    }
    let mut child = command.spawn().expect("omp must be on PATH");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let ready = loop {
        let frame = next_frame(&mut lines).await;
        if frame["type"] == "ready" {
            break frame;
        }
    };
    let mut assembler = None;
    if ready["supportedProtocolVersions"]
        .as_array()
        .is_some_and(|v| v.iter().any(|v| v == 2))
    {
        let frame = json!({"id": "neg", "type": "negotiate_protocol", "protocolVersion": 2});
        stdin
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let frame = next_frame(&mut lines).await;
            if frame["type"] == "response" && frame["id"] == "neg" {
                if frame["success"] == true {
                    assembler = Some(ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED));
                }
                break;
            }
        }
    }
    let frame = json!({"id": "memory-1", "type": "prompt", "message": "/memory stats"});
    stdin
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
    let mut seen = Vec::new();
    loop {
        let mut frame = next_frame(&mut lines).await;
        if let Some(assembler) = assembler.as_mut() {
            match assembler.feed(frame) {
                Reassembled::Frame(whole) => frame = whole,
                _ => continue,
            }
        }
        let ty = frame["type"].as_str().unwrap_or("").to_string();
        if matches!(
            ty.as_str(),
            "command_output" | "prompt_result" | "message_end"
        ) || (ty == "response" && frame["id"] == "memory-1")
        {
            let done = ty != "command_output";
            seen.push(frame);
            if done {
                break;
            }
        }
    }
    drop(stdin);
    let _ = child.wait().await;
    let _ = std::fs::remove_file(overlay_path);
    let text = seen
        .iter()
        .filter(|f| f["type"] == "command_output")
        .filter_map(|f| f["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (
        memory_stats_verdict(&seen, "memory-1", db, &cfg.role, home),
        text,
    )
}

/// No model call. The real omp, under the adapter's own flags and overlay
/// for a `memory: true` role, opens its store under the temp state folder
/// and nothing of the operator's omp state changes.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_opens_the_memory_store -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real omp on PATH (no model call is made)"]
async fn omp_opens_the_memory_store_without_touching_the_operators_state() {
    let env = Env::new();
    let cfg = env.memory_cfg("P");
    let before = omp_home_state().await;
    let adapter = OmpAdapter::with_binary("omp", &env.state);
    let db = adapter.prepare_memory_store(&cfg).unwrap();
    let (verdict, text) = real_memory_stats(&env, &cfg, &db, None).await;
    println!("{text}");
    verdict.unwrap();
    assert!(
        db.exists(),
        "mnemopi.db was not created at {}",
        db.display()
    );
    assert_eq!(omp_home_state().await, before);
}

/// No model call. The store sits under the `HOME` omp runs with (a temp
/// one, so the operator's `~/.omp` is untouched), as production keeps it;
/// real omp prints that path as `~/…` and the check must still pass.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_opens_a_memory_store_under_its_home -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real omp on PATH (no model call is made)"]
async fn omp_opens_a_memory_store_under_its_home() {
    let env = Env::new();
    let home = std::fs::canonicalize(&env.dir).unwrap();
    let state = home.join(".config/chocofactory/omp");
    let cfg = env.memory_cfg("P");
    let adapter = OmpAdapter::with_binary("omp", &state);
    let db = adapter.prepare_memory_store(&cfg).unwrap();
    assert!(db.starts_with(&home));
    let env = Env {
        dir: env.dir.clone(),
        state,
        repo: env.repo.clone(),
    };
    let (verdict, text) = real_memory_stats(&env, &cfg, &db, Some(&home)).await;
    println!("{text}");
    verdict.unwrap();
    assert!(
        db.exists(),
        "mnemopi.db was not created at {}",
        db.display()
    );
}

/// No model call. With the store folder unwritable, omp still starts and
/// claims memory; only the check notices.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_with_an_unwritable_memory_store -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real omp on PATH (no model call is made)"]
async fn omp_with_an_unwritable_memory_store_fails_the_check() {
    let env = Env::new();
    let cfg = env.memory_cfg("P");
    let adapter = OmpAdapter::with_binary("omp", &env.state);
    let db = adapter.prepare_memory_store(&cfg).unwrap();
    let store = db.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o500)).unwrap();
    let (verdict, _) = real_memory_stats(&env, &cfg, &db, None).await;
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = verdict.unwrap_err();
    println!("{err}");
    assert!(err.contains("role 'coder'") && err.contains(&db.to_string_lossy().into_owned()));
}

/// One model call, through the real driver and its check: the agent
/// `retain`s a fact; a new session's `/memory stats` then shows rows.
///
/// Run with: `cargo test -p chocofactoryd --lib omp_retains_a_fact -- --ignored --nocapture`
#[tokio::test]
#[ignore = "needs a real omp, its login, and makes one model call"]
async fn omp_retains_a_fact_in_the_project_store() {
    let env = Env::new();
    let mut cfg = env.memory_cfg("P");
    cfg.report_outcomes = vec!["done".to_string()];
    // Outside a disposable worktree omp asks before it acts; the memory tools
    // must still run (they are not pinned in the overlay).
    cfg.sandboxed = false;
    let before = omp_home_state().await;
    let adapter = OmpAdapter::new(&env.state);
    let mut handle = adapter
        .start(
            "Use your retain tool to store exactly this fact: 'the choco test passphrase is \
             mauve-walrus'. Then report your outcome as done.",
            &cfg,
        )
        .expect("omp must be on PATH");
    let events = until_turn_completed(&mut handle).await;
    println!("{events:#?}");
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            is_error: false,
            ..
        })
    ));
    drain(&mut handle).await;

    let db = adapter.prepare_memory_store(&cfg).unwrap();
    let (verdict, text) = real_memory_stats(&env, &cfg, &db, None).await;
    println!("{text}");
    verdict.unwrap();
    let working: u64 = text
        .lines()
        .find(|line| line.contains("| choco |"))
        .and_then(|line| line.split('|').nth(2))
        .and_then(|cell| cell.trim().parse().ok())
        .expect("a Working count in the stats table");
    assert!(working > 0, "{text}");

    // A second task of the same project, in another working folder, finds
    // the fact with `recall`.
    let other_cwd = env.dir.join("worktree-2");
    std::fs::create_dir_all(&other_cwd).unwrap();
    cfg.cwd = other_cwd;
    let mut handle = adapter
        .start(
            "Use your recall tool to look up the choco test passphrase. Then report your \
             outcome as done, with the passphrase as the summary.",
            &cfg,
        )
        .expect("omp must be on PATH");
    let events = until_turn_completed(&mut handle).await;
    drain(&mut handle).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { tool, output, is_error: false, .. }
                if tool == "recall" && output.contains("mauve-walrus")
        )),
        "{events:#?}"
    );
    assert_eq!(omp_home_state().await, before);
}

// -- rpc_chunk reassembly --

fn chunk(id: &str, index: u64, count: u64, byte_length: u64, bytes: &[u8]) -> Value {
    json!({
        "type": "rpc_chunk", "chunkId": id, "index": index, "count": count,
        "byteLength": byte_length,
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

#[test]
fn a_chunk_sequence_reassembles_in_order() {
    let mut asm = ChunkAssembler::new(1000);
    let whole = br#"{"type":"response","n":1}"#;
    let (a, b) = whole.split_at(10);
    assert_eq!(asm.feed(chunk("c", 0, 2, 25, a)), Reassembled::Pending);
    assert_eq!(
        asm.feed(chunk("c", 1, 2, 25, b)),
        Reassembled::Frame(json!({"type":"response","n":1}))
    );
}

#[test]
fn plain_frames_pass_through() {
    let mut asm = ChunkAssembler::new(1000);
    assert_eq!(
        asm.feed(json!({"type": "x"})),
        Reassembled::Frame(json!({"type": "x"}))
    );
}

#[test]
fn bad_sequences_are_rejected() {
    let whole = br#"{"a":1}"#;
    let new = || ChunkAssembler::new(1000);
    // Interleaved.
    let mut asm = new();
    assert_eq!(
        asm.feed(chunk("c", 0, 2, 7, &whole[..3])),
        Reassembled::Pending
    );
    assert!(matches!(
        asm.feed(chunk("d", 0, 2, 7, &whole[..3])),
        Reassembled::Rejected(_)
    ));
    // Out of order.
    let mut asm = new();
    assert_eq!(
        asm.feed(chunk("c", 0, 3, 7, &whole[..2])),
        Reassembled::Pending
    );
    assert!(matches!(
        asm.feed(chunk("c", 2, 3, 7, &whole[2..])),
        Reassembled::Rejected(_)
    ));
    // Starting mid-sequence.
    assert!(matches!(
        new().feed(chunk("c", 1, 2, 7, whole)),
        Reassembled::Rejected(_)
    ));
    // Over the limit.
    assert!(matches!(
        ChunkAssembler::new(5).feed(chunk("c", 0, 1, 7, whole)),
        Reassembled::Rejected(_)
    ));
    // Wrong declared length.
    assert!(matches!(
        new().feed(chunk("c", 0, 1, 9, whole)),
        Reassembled::Rejected(_)
    ));
    // Not UTF-8, not JSON, not an object.
    assert!(matches!(
        new().feed(chunk("c", 0, 1, 2, &[0xff, 0xfe])),
        Reassembled::Rejected(_)
    ));
    assert!(matches!(
        new().feed(chunk("c", 0, 1, 3, b"abc")),
        Reassembled::Rejected(_)
    ));
    assert!(matches!(
        new().feed(chunk("c", 0, 1, 1, b"1")),
        Reassembled::Rejected(_)
    ));
    // Missing fields.
    assert!(matches!(
        new().feed(json!({"type": "rpc_chunk"})),
        Reassembled::Rejected(_)
    ));
}

#[test]
fn a_plain_frame_interrupts_a_sequence_and_is_delivered() {
    let mut asm = ChunkAssembler::new(1000);
    assert_eq!(
        asm.feed(chunk("c", 0, 2, 7, b"{\"a\"")),
        Reassembled::Pending
    );
    assert_eq!(
        asm.feed(json!({"type": "x"})),
        Reassembled::Frame(json!({"type": "x"}))
    );
    // The interrupted sequence is gone: its continuation starts mid-way.
    assert!(matches!(
        asm.feed(chunk("c", 1, 2, 7, b":1}")),
        Reassembled::Rejected(_)
    ));
}

// -- turns that must never hang --

#[tokio::test]
async fn a_rejected_prompt_ends_the_turn_with_its_error_exactly_once() {
    for mode in ["reject_prompt", "reject_prompt_late"] {
        let env = Env::new();
        let adapter = env.adapter(&[("FAKE_OMP_MODES", mode)]);
        let mut handle = adapter.start("/skill:nope", &env.cfg()).unwrap();
        let mut events = until_turn_completed(&mut handle).await;
        events.extend(drain(&mut handle).await);
        let errors: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Error { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(errors, vec!["no such skill: nope"], "{mode}");
        let completions = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
            .count();
        assert_eq!(completions, 1, "{mode}");
        let error_at = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Error { .. }))
            .unwrap();
        assert!(
            matches!(
                events[error_at + 1],
                AgentEvent::TurnCompleted { is_error: true, .. }
            ),
            "{mode}: {events:?}"
        );
    }
}

#[tokio::test]
async fn a_rejected_prompt_without_an_error_field_gets_the_fallback_text() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "reject_noerror")]);
    let mut handle = adapter.start("/skill:nope", &env.cfg()).unwrap();
    let mut events = until_turn_completed(&mut handle).await;
    events.extend(drain(&mut handle).await);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message } if message == "omp rejected the prompt"
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_rejected_follow_up_does_not_end_the_running_turn() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "reject_followup")]);
    let mut handle = adapter.start("one", &env.cfg()).unwrap();
    handle.send("/skill:nope").unwrap();
    let mut events = until_turn_completed(&mut handle).await;
    events.extend(drain(&mut handle).await);
    let completions: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnCompleted { is_error, usage } => Some((*is_error, usage.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(completions.len(), 1, "{events:?}");
    assert!(!completions[0].0, "{events:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message } if message == "no such skill: nope"
        )),
        "{events:?}"
    );
    let error_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Error { .. }))
        .unwrap();
    let done_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        .unwrap();
    assert!(error_at < done_at, "{events:?}");
}

#[tokio::test]
async fn a_rejected_follow_up_does_not_end_a_turn_waiting_to_settle() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unsettled,reject_unsettled")]);
    let mut handle = adapter.start("one", &env.cfg()).unwrap();
    handle.send("/skill:nope").unwrap();
    let mut events = until_turn_completed(&mut handle).await;
    events.extend(drain(&mut handle).await);
    let completions: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnCompleted { is_error, usage } => Some((*is_error, usage.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(completions.len(), 1, "{events:?}");
    assert!(!completions[0].0, "{events:?}");
    let error_at = events
        .iter()
        .position(
            |e| matches!(e, AgentEvent::Error { message } if message == "no such skill: nope"),
        )
        .expect("the rejection is reported");
    let done_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        .unwrap();
    assert!(error_at < done_at, "{events:?}");
    // The running turn keeps its own messages, background run included.
    assert!(
        events[..done_at].iter().any(|e| matches!(
            e,
            AgentEvent::AssistantMessage { text } if text == "background result"
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_statistics_timeout_still_starts_the_next_queued_completion() {
    let env = Env::new();
    // Call 0 is the baseline; the first turn's reading never answers.
    let adapter = env.adapter(&[
        ("FAKE_OMP_MODES", "noreport"),
        ("FAKE_OMP_STATS_SEQ", "ok,silent,ok"),
    ]);
    let mut handle = adapter.start("one", &env.cfg()).unwrap();
    handle.send("two").unwrap();
    let first = until_turn_completed(&mut handle).await;
    assert!(matches!(
        first.last(),
        Some(AgentEvent::TurnCompleted { .. })
    ));
    let second = until_turn_completed(&mut handle).await;
    assert!(matches!(
        second.last(),
        Some(AgentEvent::TurnCompleted { .. })
    ));
    drain(&mut handle).await;
}

#[tokio::test(start_paused = true)]
async fn a_session_that_never_starts_ends_the_stream_and_kills_the_process() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "no_ready")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let mut events = Vec::new();
    while let Some(event) = handle.recv().await {
        events.push(event);
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message } if message.contains("ready")
        )),
        "{events:?}"
    );
    // The fake sleeps for ten minutes: it only exits because it was killed.
    let status = handle.wait().await.unwrap();
    assert!(!status.success());
}

#[tokio::test]
async fn a_host_tool_call_without_its_fields_is_answered_as_an_error() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,bad_host_call")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let events = until_turn_completed(&mut handle).await;
    let reply = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::AssistantMessage { text } => text.strip_prefix("bad-call-reply:"),
            _ => None,
        })
        .expect("the fake got a reply");
    let reply: Value = serde_json::from_str(reply).unwrap();
    assert_eq!(reply["id"], "htc-bad");
    assert_eq!(reply["isError"], true);
    drain(&mut handle).await;
}
