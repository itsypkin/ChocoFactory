//! Tests for the omp adapter, against `tests/fixtures/fake_omp.py`.
//!
//! The fake is configured through a per-test wrapper script that exports its
//! environment variables, never through this process's own environment, so
//! tests running in parallel can't affect each other.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::adapter::ClaudeAdapter;
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
    let mut script = String::from("#!/bin/sh\n");
    if let Some(marker) = marker {
        script.push_str(&format!("touch '{}'\n", marker.display()));
    }
    for (key, value) in env {
        script.push_str(&format!(
            "export {key}='{}'\n",
            value.replace('\'', "'\\''")
        ));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", fixture_binary(fixture)));
    let path = dir.join(format!("wrapper-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
            "report_outcome"
        ]
    );

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
    let started = std::time::Instant::now();
    let events = until_turn_completed(&mut handle).await;
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "completed before session_settled"
    );
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
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unsettled_die")]);
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
        report_instruction(&cfg.report_outcomes)
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
    let tools = |disallowed: &[RoleTool]| allowed_tools(disallowed).join(",");
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
    // A uniquely named variable under the prefix, so no other test can see it.
    // SAFETY: only this test reads or writes this one name.
    unsafe { std::env::set_var("OTEL_CHOCO_SCRUB_TEST", "x") };
    let mut command = Command::new("true");
    scrub_env(&mut command);
    let removed = command
        .as_std()
        .get_envs()
        .any(|(k, v)| k == "OTEL_CHOCO_SCRUB_TEST" && v.is_none());
    assert!(removed);
    unsafe { std::env::remove_var("OTEL_CHOCO_SCRUB_TEST") };
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
    assert_eq!(models[0].tokens.input, Some(300));
    close(models[0].cost_usd, 0.0369);
    assert!(usage.wall_time_ms.is_some());
    drain(&mut handle).await;
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
    drain(&mut handle).await;
}

#[tokio::test]
async fn an_unpriced_model_has_no_cost() {
    let env = Env::new();
    let adapter = env.adapter(&[("FAKE_OMP_MODES", "noreport,unpriced")]);
    let mut handle = adapter.start("go", &env.cfg()).unwrap();
    let usage = usage_of(&until_turn_completed(&mut handle).await);
    assert_eq!(usage.cost_usd, None);
    assert_eq!(usage.models.unwrap()[0].cost_usd, None);
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
        // Models and model turns come from the messages, not the reading.
        assert_eq!(usage.model_turns, Some(3));
        assert!(usage.models.is_some());
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
fn validate_role_rejects_memory_and_pattern_skill_names() {
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
    assert_eq!(
        adapter
            .validate_role("coder", &isolated(&[], true))
            .unwrap_err(),
        "role 'coder' runs on cli 'omp', which can't use memory: true; remove memory: true or \
         run the role on cli: claude"
    );
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
// Opt-in tests against the real `omp` (ignored by default)
// ---------------------------------------------------------------------------

/// Everything `spawn` does before starting the process, for tests that talk
/// to the real omp themselves: the overlay on disk, the arguments, and the
/// scrubbed command.
fn real_command(binary: &str, state: &Path, cfg: &RoleConfig) -> (Command, PathBuf) {
    let (files, _warnings) = read_repo_instructions(&cfg.cwd);
    let append = match &cfg.isolation {
        Isolation::Isolated { .. } => Some(append_block(&render_instruction_files(&files), cfg)),
        Isolation::InheritOperatorConfig => None,
    };
    let overlay_dir = state.join("overlays");
    let session_dir = state.join("sessions");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    let overlay_path = overlay_dir.join(format!("{}.yml", uuid::Uuid::new_v4()));
    write_private_file(
        &overlay_path,
        &serde_yaml::to_string(&overlay(cfg)).unwrap(),
    )
    .unwrap();
    let args = build_args(cfg, &overlay_path, &session_dir, append.as_deref(), None);
    let mut command = Command::new(binary);
    command
        .current_dir(&cfg.cwd)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    scrub_env(&mut command);
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
    let (mut command, overlay_path) = real_command("omp", &env.state, &cfg);
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
    let (command, overlay_path) = real_command("omp", &state, &cfg);
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
