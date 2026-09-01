use std::collections::HashMap;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{AdapterError, AgentAdapter, AgentEvent, AgentHandle, RoleConfig};

/// Wraps `claude --print --output-format=stream-json --input-format=stream-json
/// [--permission-mode=bypassPermissions] [--resume <id>]` as a subprocess
/// (§4) — the permission flag only when `RoleConfig.sandboxed` says `cwd`
/// is a disposable worktree (#67). Every turn — including the first — is
/// sent as a stream-json user-turn line over stdin, so
/// `start`/`resume`/`AgentHandle::send` all go through the same path.
pub struct ClaudeAdapter {
    binary: String,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self {
            binary: "claude".to_string(),
        }
    }

    /// Points at a different executable. Used by tests to substitute a
    /// fake CLI for the real `claude` binary.
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn start(&self, prompt: &str, cfg: &RoleConfig) -> Result<AgentHandle, AdapterError> {
        spawn(&self.binary, cfg, None, prompt)
    }

    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
    ) -> Result<AgentHandle, AdapterError> {
        spawn(&self.binary, cfg, Some(session_id), prompt)
    }
}

fn spawn(
    binary: &str,
    cfg: &RoleConfig,
    resume_session_id: Option<&str>,
    initial_prompt: &str,
) -> Result<AgentHandle, AdapterError> {
    let mut command = Command::new(binary);
    command
        .current_dir(&cfg.cwd)
        .arg("--print")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so `session::SessionManager::cancel` can
        // signal the *whole tree* on cancel (#69) rather than just the
        // `claude` the daemon spawned. An agent turn's real weight is in
        // what it starts — a `npm test`, a dev server, a build — and
        // killing only the parent would leave those running in the task's
        // working copy after the operator was told the task was cancelled.
        // Same reasoning, and the same `killpg` helper, as a `shell`
        // stage's timeout (`shell::run`).
        //
        // The tradeoff is the one `shell.rs` already documents: a child in
        // its own group no longer receives the terminal's signals, so
        // Ctrl-C on a foreground daemon reaches the daemon but not the
        // agent. The daemon installs no shutdown handler today, so
        // `kill_on_drop` doesn't run on exit either and such a process
        // already outlives it; this doesn't make that worse, and the real
        // fix is graceful shutdown, which is not this change's to make.
        .process_group(0)
        .kill_on_drop(true);

    // `claude`'s normal permission model expects a human to approve each
    // edit/write interactively — there is no human on the other end of
    // this stdin pipe, ever, so without this every write stalls on an
    // approval that can structurally never arrive (issue #67: observed
    // live, a coder turn spent 4+ minutes and dozens of tool calls trying
    // every workaround before giving up entirely). Only safe when `cwd`
    // is the disposable, isolated worktree a workflow opted into
    // (`cfg.sandboxed`, §5.5 Q7, issue #58) — exactly the "sandbox"
    // scenario `claude --help` names as this flag's intended use;
    // `cfg.sandboxed == false` means `cwd` is the task's real configured
    // repo (or the daemon's own cwd, for a workflow like `chat` that has
    // no repo at all), where bypassing every permission check would be a
    // straightforward security regression instead of a safe default.
    if cfg.sandboxed {
        command.arg("--permission-mode").arg("bypassPermissions");
    }

    if let Some(model) = &cfg.model {
        command.arg("--model").arg(model);
    }
    if let Some(system_prompt) = &cfg.system_prompt {
        command.arg("--system-prompt").arg(system_prompt);
    }
    if let Some(session_id) = resume_session_id {
        command.arg("--resume").arg(session_id);
    }

    let mut child = command.spawn().map_err(AdapterError::Spawn)?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();
    let (events_tx, events_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // The initial prompt goes through the same stdin channel as any
    // later `AgentHandle::send`, since claude accepts every turn
    // (including the first) as a stream-json line once
    // --input-format=stream-json is set.
    stdin_tx
        .send(initial_prompt.to_string())
        .expect("stdin_rx not yet dropped");

    tokio::spawn(run_stdin_writer(stdin, stdin_rx));
    tokio::spawn(run_stderr_reader(stderr, events_tx.clone()));
    tokio::spawn(run_stdout_reader(stdout, events_tx));

    Ok(AgentHandle::new(child, events_rx, stdin_tx))
}

async fn run_stdin_writer(
    mut stdin: tokio::process::ChildStdin,
    mut stdin_rx: mpsc::UnboundedReceiver<String>,
) {
    while let Some(text) = stdin_rx.recv().await {
        let line = user_turn_line(&text);
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn run_stderr_reader(
    stderr: tokio::process::ChildStderr,
    events_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if events_tx.send(AgentEvent::Error { message: line }).is_err() {
            return;
        }
    }
}

async fn run_stdout_reader(
    stdout: tokio::process::ChildStdout,
    events_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for event in normalize(&value, &mut tool_names) {
            if events_tx.send(event).is_err() {
                return;
            }
        }
    }
}

fn user_turn_line(text: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
    });
    format!("{msg}\n")
}

/// Translates one line of claude's native stream-json output into zero or
/// more `AgentEvent`s (§4.2). `tool_names` correlates a later `tool_result`
/// back to the tool name from its matching `tool_use` block, since the
/// result block only carries the call's id.
fn normalize(value: &Value, tool_names: &mut HashMap<String, String>) -> Vec<AgentEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            vec![AgentEvent::SessionMeta { session_id }]
        }
        Some("assistant") => normalize_assistant(value, tool_names),
        Some("user") => normalize_user(value, tool_names),
        Some("result") => normalize_result(value),
        _ => Vec::new(),
    }
}

fn normalize_assistant(value: &Value, tool_names: &mut HashMap<String, String>) -> Vec<AgentEvent> {
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    out.push(AgentEvent::AssistantMessage {
                        text: text.to_string(),
                    });
                }
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    out.push(AgentEvent::Thinking {
                        text: text.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let tool_use_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let tool = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                tool_names.insert(tool_use_id.clone(), tool.clone());
                out.push(AgentEvent::ToolCall {
                    tool_use_id,
                    tool,
                    input,
                });
            }
            _ => {}
        }
    }
    out
}

fn normalize_user(value: &Value, tool_names: &HashMap<String, String>) -> Vec<AgentEvent> {
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let tool_use_id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tool = tool_names.get(&tool_use_id).cloned().unwrap_or_default();
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let output = match block.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        out.push(AgentEvent::ToolResult {
            tool_use_id,
            tool,
            output,
            is_error,
        });
    }
    out
}

// A `result` line always means the same thing regardless of `is_error`: the
// CLI is done with this turn and is now only waiting on stdin EOF to
// exit — it never exits on its own (#70). Both outcomes therefore emit
// `TurnCompleted`, so a single-shot turn's stdin gets closed either way;
// only a clean finish (`is_error: false`) also counts as *completion* for
// `drain_session`'s purposes, which is why the flag rides along rather than
// being collapsed here.
fn normalize_result(value: &Value) -> Vec<AgentEvent> {
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_error {
        return vec![AgentEvent::TurnCompleted { is_error: false }];
    }
    let message = value
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "agent run ended with an error".to_string());
    vec![
        AgentEvent::Error { message },
        AgentEvent::TurnCompleted { is_error: true },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    // Fixtures below are real `claude --print --output-format=stream-json
    // --verbose` output, captured by hand while building this adapter.

    #[test]
    fn normalizes_system_init_to_session_meta() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"9bf8db32-b723-41f6-8963-ea3ece07cb1a","tools":["Bash"],"model":"claude-sonnet-5"}"#;
        let mut tool_names = HashMap::new();
        let events = normalize(&parse(line), &mut tool_names);
        assert_eq!(
            events,
            vec![AgentEvent::SessionMeta {
                session_id: "9bf8db32-b723-41f6-8963-ea3ece07cb1a".to_string()
            }]
        );
    }

    #[test]
    fn normalizes_assistant_text_block() {
        let line = r#"{"type":"assistant","message":{"model":"claude-sonnet-5","id":"msg_01FpcJagDvX9Hv2LF9yLsdF7","type":"message","role":"assistant","content":[{"type":"text","text":"pong"}],"stop_reason":null},"session_id":"9bf8db32-b723-41f6-8963-ea3ece07cb1a"}"#;
        let mut tool_names = HashMap::new();
        let events = normalize(&parse(line), &mut tool_names);
        assert_eq!(
            events,
            vec![AgentEvent::AssistantMessage {
                text: "pong".to_string()
            }]
        );
    }

    #[test]
    fn normalizes_tool_use_then_correlates_tool_result() {
        let tool_use_line = r#"{"type":"assistant","message":{"model":"claude-sonnet-5","id":"msg_01WvqJHKW4mdw38SwrhHh7kR","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_01115SPXiWWzz1P1dPhHbWAe","name":"Bash","input":{"command":"echo hello-from-tool","description":"Print test string to stdout"}}]},"session_id":"0259e0c8-5b32-4044-a69a-4bd21257621d"}"#;
        let tool_result_line = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01115SPXiWWzz1P1dPhHbWAe","type":"tool_result","content":"hello-from-tool","is_error":false}]},"session_id":"0259e0c8-5b32-4044-a69a-4bd21257621d"}"#;

        let mut tool_names = HashMap::new();
        let call_events = normalize(&parse(tool_use_line), &mut tool_names);
        assert_eq!(
            call_events,
            vec![AgentEvent::ToolCall {
                tool_use_id: "toolu_01115SPXiWWzz1P1dPhHbWAe".to_string(),
                tool: "Bash".to_string(),
                input: serde_json::json!({
                    "command": "echo hello-from-tool",
                    "description": "Print test string to stdout",
                }),
            }]
        );

        let result_events = normalize(&parse(tool_result_line), &mut tool_names);
        assert_eq!(
            result_events,
            vec![AgentEvent::ToolResult {
                tool_use_id: "toolu_01115SPXiWWzz1P1dPhHbWAe".to_string(),
                tool: "Bash".to_string(),
                output: "hello-from-tool".to_string(),
                is_error: false,
            }]
        );
    }

    #[test]
    fn normalizes_successful_result_to_turn_completed() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","session_id":"9bf8db32-b723-41f6-8963-ea3ece07cb1a"}"#;
        let mut tool_names = HashMap::new();
        assert_eq!(
            normalize(&parse(line), &mut tool_names),
            vec![AgentEvent::TurnCompleted { is_error: false }]
        );
    }

    #[test]
    fn normalizes_error_result_to_error_event_then_turn_completed() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","session_id":"abc"}"#;
        let mut tool_names = HashMap::new();
        assert_eq!(
            normalize(&parse(line), &mut tool_names),
            vec![
                AgentEvent::Error {
                    message: "boom".to_string()
                },
                AgentEvent::TurnCompleted { is_error: true },
            ]
        );
    }

    #[test]
    fn ignores_rate_limit_events() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"},"session_id":"abc"}"#;
        let mut tool_names = HashMap::new();
        assert_eq!(normalize(&parse(line), &mut tool_names), Vec::new());
    }

    fn fixture_binary(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn start_spawns_process_and_streams_events() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
        };
        let mut handle = adapter.start("hello", &cfg).unwrap();

        let first = handle.recv().await.unwrap();
        assert!(matches!(first, AgentEvent::SessionMeta { .. }));

        let second = handle.recv().await.unwrap();
        assert_eq!(
            second,
            AgentEvent::AssistantMessage {
                text: "echo:hello".to_string()
            }
        );

        // fake_claude.py emits a `result` line after every reply, exactly
        // like the real CLI (#70) — normalized to `TurnCompleted` rather
        // than discarded.
        let third = handle.recv().await.unwrap();
        assert_eq!(third, AgentEvent::TurnCompleted { is_error: false });

        handle.send("again").unwrap();
        let fourth = handle.recv().await.unwrap();
        assert_eq!(
            fourth,
            AgentEvent::AssistantMessage {
                text: "echo:again".to_string()
            }
        );

        drop(handle);
    }

    #[tokio::test]
    async fn resume_passes_session_id_through_to_the_cli() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
        };
        let mut handle = adapter
            .resume("fixed-session-id", "hello again", &cfg)
            .unwrap();

        let first = handle.recv().await.unwrap();
        assert_eq!(
            first,
            AgentEvent::SessionMeta {
                session_id: "fixed-session-id".to_string()
            }
        );
    }

    /// #67: `claude`'s normal permission model expects a human to approve
    /// each edit interactively, which can never happen on the other end of
    /// this stdin pipe — a sandboxed spawn (`cfg.sandboxed`, a disposable
    /// worktree the workflow opted into, §5.5 Q7/#58) opts out of it.
    #[tokio::test]
    async fn a_sandboxed_spawn_bypasses_claudes_own_permission_prompts() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: true,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        handle.recv().await.unwrap(); // SessionMeta
        let reply = handle.recv().await.unwrap();
        assert_eq!(
            reply,
            AgentEvent::AssistantMessage {
                text: "model=<unset>|system_prompt=<unset>|permission_mode=bypassPermissions"
                    .to_string()
            }
        );
    }

    /// The other half of #67: a task whose workflow never opted into a
    /// disposable worktree (`chat`, or any custom definition that omits
    /// `worktree: true`) must keep `claude`'s own permission prompts
    /// enabled — `cwd` there is the task's real configured repo, or the
    /// daemon's own cwd, neither of which bypassing every edit check is
    /// safe against.
    #[tokio::test]
    async fn an_unsandboxed_spawn_leaves_claudes_permission_prompts_enabled() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        handle.recv().await.unwrap(); // SessionMeta
        let reply = handle.recv().await.unwrap();
        assert_eq!(
            reply,
            AgentEvent::AssistantMessage {
                text: "model=<unset>|system_prompt=<unset>|permission_mode=<unset>".to_string()
            }
        );
    }
}
