use std::collections::HashMap;
use std::process::Stdio;

use chocofactory_core::mcp::MCP_SERVER_NAME;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{
    AdapterError, AgentAdapter, AgentEvent, AgentHandle, InterruptionEvidence, Isolation,
    RoleConfig,
};

/// Wraps `claude --print --output-format=stream-json --input-format=stream-json
/// [--permission-mode=bypassPermissions] --mcp-config <...> [isolation flags]
/// [--append-system-prompt <...>] [--resume <id>]` as a subprocess (§4) — the
/// permission flag only when `RoleConfig.sandboxed` says `cwd` is a disposable
/// worktree (#67), the mcp config always (issue #73's `report_outcome` tool),
/// the isolation flags unless the role inherits the operator's setup (#90),
/// the append-prompt only when the stage has outcomes to report. Every turn — including the
/// first — is sent as a stream-json user-turn line over stdin, so
/// `start`/`resume`/`AgentHandle::send` all go through the same path.
pub struct ClaudeAdapter {
    binary: String,
    choco_binary: String,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self {
            binary: "claude".to_string(),
            choco_binary: default_choco_binary(),
        }
    }

    /// Points at a different executable. Used by tests to substitute a
    /// fake CLI for the real `claude` binary.
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            choco_binary: default_choco_binary(),
        }
    }

    /// Overrides the `choco` binary path used to build `--mcp-config`'s
    /// stdio command (issue #73), mirroring `with_binary`'s override of
    /// `claude` itself. Used by the daemon's `CHOCOFACTORY_CHOCO_BINARY` env
    /// override, and by tests that want a deterministic path to assert
    /// against rather than whatever `default_choco_binary` resolves to in a
    /// given build layout.
    pub fn with_choco_binary(mut self, choco_binary: impl Into<String>) -> Self {
        self.choco_binary = choco_binary.into();
        self
    }
}

/// Locates `choco` as `current_exe()`'s sibling — the layout both
/// `target/debug/` (cargo) and an installed `bin/` directory share, and the
/// same trick `choco/tests/cli.rs`'s `workspace_binary` already uses to find
/// its own sibling binaries. Falls back to the bare name, resolved via
/// `PATH` when the command actually runs, if the running executable's own
/// path can't be read — an embedding unusual enough that this daemon
/// doesn't otherwise support it either.
fn default_choco_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("choco")))
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| "choco".to_string())
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn start(&self, prompt: &str, cfg: &RoleConfig) -> Result<AgentHandle, AdapterError> {
        spawn(&self.binary, &self.choco_binary, cfg, None, prompt)
    }

    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
    ) -> Result<AgentHandle, AdapterError> {
        spawn(
            &self.binary,
            &self.choco_binary,
            cfg,
            Some(session_id),
            prompt,
        )
    }
}

fn spawn(
    binary: &str,
    choco_binary: &str,
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

    // Issue #73: the `report_outcome` tool is available on *every* agent
    // turn, not something wired in only for a reviewer-shaped stage — a
    // "reviewer" is just a role name, and `StageDef.on` is the whole
    // contract. `cfg.report_outcomes` (the current stage's `on:` edge names)
    // decides only what `--outcome`s the tool is launched with, not whether
    // it exists; an empty list still serves the tool, just with a free-form,
    // non-routing `outcome`.
    //
    // One `--outcome <name>` per edge, not one comma-joined flag (review,
    // #75): an `on:` edge name is an arbitrary YAML string and could itself
    // contain a comma, which no single delimiter-joined value can
    // round-trip unambiguously.
    let mut mcp_args = vec!["mcp-serve".to_string()];
    for outcome in &cfg.report_outcomes {
        mcp_args.push("--outcome".to_string());
        mcp_args.push(outcome.clone());
    }
    // Issue #95: the sections this stage's report must carry, one flag each
    // for the same round-tripping reason as `--outcome`. The tool rejects a
    // report that leaves one out, so a reviewer can't file a verdict
    // without also filing the walks it rests on.
    for section in &cfg.report_sections {
        mcp_args.push("--require-section".to_string());
        mcp_args.push(section.clone());
    }
    // `alwaysLoad` (#90): without it the CLI lists `report_outcome` as a
    // *deferred* tool whose schema has to be fetched with `ToolSearch` before
    // it can be called. Probed against Claude Code 2.1.272: even with ours as
    // the only MCP server the tool was deferred, and in #61 two reviewers
    // never loaded it at all. A single-shot turn now can't complete without
    // this call, so it has to be in the tool list from the first token.
    let mcp_config = json!({
        "mcpServers": {
            (MCP_SERVER_NAME): {
                "type": "stdio",
                "command": choco_binary,
                "args": mcp_args,
                "alwaysLoad": true,
            }
        }
    })
    .to_string();
    command.arg("--mcp-config").arg(mcp_config);

    // #90: what the turn may pick up from the operator's own machine. Each
    // flag was checked against a real session (Claude Code 2.1.272): with
    // all of them, the `init` line reported no plugins, the default output
    // style and only our MCP server, the transcript loaded only the task
    // repo's `CLAUDE.md`, and no hooks ran.
    //
    // - `--setting-sources project,local` skips user settings, which is
    //   where plugins, hooks and the output style are enabled, and also
    //   skips `~/.claude/CLAUDE.md`. The task repo's own settings and
    //   `CLAUDE.md` still apply: those belong to the code being worked on.
    // - `--strict-mcp-config` drops the operator's MCP servers, leaving only
    //   ours.
    // - `ReportFindings` is a built-in verdict tool that reviewers reached
    //   for instead of `report_outcome` (#61).
    // - With no skills allowed, the `Skill` tool is removed outright; with
    //   some, the allowlist is sent on stdin below.
    let initialize = match &cfg.isolation {
        Isolation::InheritOperatorConfig => None,
        Isolation::Isolated { skills, memory } => {
            command
                .arg("--setting-sources")
                .arg("project,local")
                .arg("--strict-mcp-config");
            let mut disallowed = vec!["ReportFindings"];
            if skills.is_empty() {
                disallowed.push("Skill");
            }
            command.arg("--disallowedTools").arg(disallowed.join(","));
            if !memory {
                command.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
            }
            Some(initialize_line(skills))
        }
    };

    // Only a stage that can conclude on its own gets outcomes (the engine
    // passes none for a standing session like chat), and every such stage
    // now has to report to complete (#90). Built from the exact same list the
    // tool's own schema uses (§ `mcp.rs`), so this instruction can never name
    // an outcome the tool would reject.
    if !cfg.report_outcomes.is_empty() {
        command
            .arg("--append-system-prompt")
            .arg(report_instruction(&cfg.report_outcomes));
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

    tokio::spawn(run_stdin_writer(stdin, initialize, stdin_rx));
    tokio::spawn(run_stderr_reader(stderr, events_tx.clone()));
    tokio::spawn(run_stdout_reader(
        stdout,
        events_tx,
        cfg.isolation.describe(),
    ));

    Ok(AgentHandle::new(child, events_rx, stdin_tx))
}

/// The instruction appended to a single-shot turn's system prompt (#90).
///
/// Says what completion *is* rather than only asking for a verdict: a turn
/// that ends without the call is treated as still working (and eventually
/// nudged), which is what lets an agent wait on its own background work
/// without the daemon mistaking that pause for "done".
fn report_instruction(outcomes: &[String]) -> String {
    format!(
        "When all of your work for this stage is finished (including anything you started \
         in the background, which you must wait for), call `report_outcome` to report the \
         stage's outcome. It must be one of: {}. Calling it is how this stage completes: \
         ending your turn without calling it means you are still working. If \
         `report_outcome` is listed as a deferred tool, load it with ToolSearch first.",
        outcomes.join(", ")
    )
}

/// The stream-json `initialize` control request carrying the turn's skills
/// allowlist (#90). This is the message the Claude Agent SDK sends for its
/// `skills` option; probed against Claude Code 2.1.272, a session given
/// `["allowed-skill"]` listed only that skill to the model. An empty list
/// allows none, which `--disallowedTools Skill` also enforces from the
/// command line.
fn initialize_line(skills: &[String]) -> String {
    let request = json!({
        "type": "control_request",
        "request_id": "chocofactory-initialize",
        "request": { "subtype": "initialize", "skills": skills },
    });
    format!("{request}\n")
}

async fn run_stdin_writer(
    mut stdin: tokio::process::ChildStdin,
    initialize: Option<String>,
    mut stdin_rx: mpsc::UnboundedReceiver<String>,
) {
    // Ahead of the first user turn, so the allowlist is in force before the
    // model ever sees a skill listing.
    if let Some(line) = initialize
        && stdin.write_all(line.as_bytes()).await.is_err()
    {
        return;
    }
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
    isolation: Value,
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
        for mut event in normalize(&value, &mut tool_names) {
            if let AgentEvent::SessionMeta {
                details: Value::Object(details),
                ..
            } = &mut event
            {
                details.insert("isolation".to_string(), isolation.clone());
            }
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
    let events = match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            vec![AgentEvent::SessionMeta {
                session_id,
                details: json!({ "init": init_summary(value) }),
            }]
        }
        Some("assistant") => normalize_assistant(value, tool_names),
        Some("user") => normalize_user(value, tool_names),
        Some("result") => normalize_result(value),
        Some("control_response") => normalize_control_response(value),
        Some("rate_limit_event") => normalize_rate_limit_event(value),
        _ => Vec::new(),
    };
    // A usage limit ends the turn from outside, and the CLI says so on the
    // assistant line that carries the limit's own text (#92) as well as in
    // the `result` that follows. Appended rather than replacing the line's
    // own events: the limit message is a real assistant message, and it is
    // what a human reading the timeline wants to see next to the marker.
    // Both lines reporting it is fine — each records which rule recognised
    // it, so seeing the structured one fire is the evidence that retires
    // the text-matching one.
    let mut events = events;
    if let Some((message, detected_by)) = assistant_interruption(value) {
        events.push(AgentEvent::Interrupted {
            message,
            detected_by,
        });
    }
    // A sub-agent's messages carry the id of the `Agent` call that spawned
    // it (#90, confirmed against a real session). Wrapped here, at the one
    // place that sees the raw line, so nothing downstream can mistake a
    // delegated helper's tool call or reply for the main agent's.
    match value.get("parent_tool_use_id").and_then(Value::as_str) {
        Some(parent) => events
            .into_iter()
            .map(|event| AgentEvent::Subagent {
                parent_tool_use_id: parent.to_string(),
                event: Box::new(event),
            })
            .collect(),
        None => events,
    }
}

/// The parts of the CLI's `system/init` line worth keeping on the timeline
/// (#90): enough to see what a turn actually ran with, so the next time the
/// tool surface shifts under a workflow it shows up in `session_meta`
/// instead of having to be dug out of the CLI's own transcript. Key names are
/// the CLI's own, as seen on a real 2.1.272 `init` line.
fn init_summary(init: &Value) -> Value {
    const KEYS: [&str; 9] = [
        "claude_code_version",
        "model",
        "permissionMode",
        "output_style",
        "tools",
        "mcp_servers",
        "plugins",
        "skills",
        "agents",
    ];
    let summary = KEYS
        .iter()
        .filter_map(|key| init.get(*key).map(|value| (key.to_string(), value.clone())))
        .collect::<serde_json::Map<_, _>>();
    Value::Object(summary)
}

/// The CLI's answer to our `initialize` request. Success is silent; a
/// failure means the skills allowlist may not be in force, which must not
/// pass unnoticed, so it becomes an `error` event on the run.
fn normalize_control_response(value: &Value) -> Vec<AgentEvent> {
    if value.pointer("/response/subtype").and_then(Value::as_str) != Some("error") {
        return Vec::new();
    }
    let detail = value
        .pointer("/response/error")
        .and_then(Value::as_str)
        .unwrap_or("no detail");
    vec![AgentEvent::Error {
        message: format!("the CLI rejected the session's initialize request: {detail}"),
    }]
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
// CLI has ended this turn and is waiting for more input or stdin EOF — it
// never exits on its own (#70). Both outcomes therefore emit
// `TurnCompleted`. Whether that turn *completed* is `drain_session`'s call:
// only a clean finish after a `report_outcome` call does (#90), which is why
// the flag rides along rather than being collapsed here.
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
    // The one place the CLI's own wording is read (#92). Everything else
    // here keys off structure; this rule exists because the `result` line is
    // the only thing the daemon is *known* to receive on a usage limit —
    // #88's whole timeline of one is `assistant`, then this. It runs last,
    // so a structured marker on the same line would have decided already.
    let first = match usage_limit_text(&message) {
        true => AgentEvent::Interrupted {
            message,
            detected_by: InterruptionEvidence::MessageText,
        },
        false => AgentEvent::Error { message },
    };
    vec![first, AgentEvent::TurnCompleted { is_error: true }]
}

/// The CLI's `rate_limit_event` line (#92). `allowed` is the ordinary
/// heartbeat and is ignored, as this adapter ignored the whole line before;
/// `rejected` is the status the real 2026-09-17 limit carried in its
/// `quotaLimits`, and means the request was refused rather than merely
/// nearing a cap.
fn normalize_rate_limit_event(value: &Value) -> Vec<AgentEvent> {
    if value
        .pointer("/rate_limit_info/status")
        .and_then(Value::as_str)
        != Some("rejected")
    {
        return Vec::new();
    }
    vec![AgentEvent::Interrupted {
        message: "the CLI reported that the account's usage limit is exhausted".to_string(),
        detected_by: InterruptionEvidence::Structured,
    }]
}

/// The machine-readable markers an assistant line carries when the API
/// refused the request for a usage limit (#92), as recorded in the CLI's own
/// transcript of the #88 turn: `"error": "rate_limit"` alongside
/// `"isApiErrorMessage": true` and `"apiErrorStatus": 429`. They sit beside
/// `message`, not inside it, so they survive whatever the message says.
fn assistant_interruption(value: &Value) -> Option<(String, InterruptionEvidence)> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let rate_limited = value.get("error").and_then(Value::as_str) == Some("rate_limit")
        || value.get("apiErrorStatus").and_then(Value::as_i64) == Some(429);
    if !rate_limited {
        return None;
    }
    // The limit's own text when there is one (it names when the limit
    // resets, which is the useful part), and a plain statement when there
    // isn't — never a silent marker with no message.
    let message = value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
            })
        })
        .unwrap_or("the CLI reported that the account's usage limit is exhausted")
        .to_string();
    Some((message, InterruptionEvidence::Structured))
}

/// Whether an error message reads like a usage limit rather than a failure
/// the agent caused. Matched case-insensitively against the phrasings seen
/// on a real limit (`You've hit your session limit · resets 3:40pm`) and the
/// API's own wording. Brittle by construction — see `normalize_result`.
fn usage_limit_text(message: &str) -> bool {
    const PHRASES: [&str; 4] = [
        "session limit",
        "usage limit",
        "rate limit",
        "rate_limit_error",
    ];
    let message = message.to_lowercase();
    PHRASES.iter().any(|phrase| message.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chocofactory_core::models::EventType;

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
                session_id: "9bf8db32-b723-41f6-8963-ea3ece07cb1a".to_string(),
                details: json!({ "init": { "tools": ["Bash"], "model": "claude-sonnet-5" } }),
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
    fn an_assistant_line_flagged_rate_limit_is_an_interruption() {
        // The shape the CLI recorded for #88's interrupted turn: the limit's
        // own text in the message, and `error`/`apiErrorStatus` beside it.
        let line = r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 3:40pm (Europe/Berlin)"}]},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"session_id":"s"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new()),
            vec![
                AgentEvent::AssistantMessage {
                    text: "You've hit your session limit · resets 3:40pm (Europe/Berlin)"
                        .to_string(),
                },
                AgentEvent::Interrupted {
                    message: "You've hit your session limit · resets 3:40pm (Europe/Berlin)"
                        .to_string(),
                    detected_by: InterruptionEvidence::Structured,
                },
            ]
        );
    }

    #[test]
    fn a_429_assistant_line_without_text_still_says_what_happened() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[]},"apiErrorStatus":429,"session_id":"s"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new()),
            vec![AgentEvent::Interrupted {
                message: "the CLI reported that the account's usage limit is exhausted".to_string(),
                detected_by: InterruptionEvidence::Structured,
            }]
        );
    }

    #[test]
    fn an_ordinary_assistant_line_is_not_an_interruption() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on it"}]},"session_id":"s"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new()),
            vec![AgentEvent::AssistantMessage {
                text: "working on it".to_string()
            }]
        );
    }

    #[test]
    fn a_rejected_rate_limit_event_is_an_interruption() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1789652400},"session_id":"abc"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new()),
            vec![AgentEvent::Interrupted {
                message: "the CLI reported that the account's usage limit is exhausted".to_string(),
                detected_by: InterruptionEvidence::Structured,
            }]
        );
    }

    #[test]
    fn an_error_result_naming_a_limit_is_an_interruption_by_its_text() {
        // The only part of #88's timeline the daemon is *known* to have
        // received. Recognised by text, and labelled as such.
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You've hit your session limit · resets 3:40pm (Europe/Berlin)","session_id":"abc"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new()),
            vec![
                AgentEvent::Interrupted {
                    message: "You've hit your session limit · resets 3:40pm (Europe/Berlin)"
                        .to_string(),
                    detected_by: InterruptionEvidence::MessageText,
                },
                AgentEvent::TurnCompleted { is_error: true },
            ]
        );
    }

    #[test]
    fn a_sub_agents_limit_line_stays_the_sub_agents() {
        // Wrapped like everything else a sub-agent emits (#90), so a
        // delegated helper's failure can't end the main turn's run as an
        // interruption on its own. The main agent's own `result` reports it
        // again when the limit really does stop the turn.
        let line = r#"{"type":"assistant","parent_tool_use_id":"toolu_agent","message":{"role":"assistant","content":[]},"error":"rate_limit","session_id":"s"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new()),
            vec![AgentEvent::Subagent {
                parent_tool_use_id: "toolu_agent".to_string(),
                event: Box::new(AgentEvent::Interrupted {
                    message: "the CLI reported that the account's usage limit is exhausted"
                        .to_string(),
                    detected_by: InterruptionEvidence::Structured,
                }),
            }]
        );
    }

    #[test]
    fn the_interruption_payload_records_which_rule_recognised_it() {
        let event = AgentEvent::Interrupted {
            message: "You've hit your session limit".to_string(),
            detected_by: InterruptionEvidence::MessageText,
        };
        assert_eq!(event.event_type(), EventType::Error);
        assert_eq!(
            event.payload(),
            json!({
                "message": "You've hit your session limit",
                "interrupted": "usage_limit",
                "detected_by": "message_text",
            })
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

    /// The first `AssistantMessage` the handle yields, skipping everything
    /// before it: the session's `SessionMeta`, and the `report_outcome` call
    /// a single-shot fixture makes before replying (#90).
    async fn next_assistant_message(handle: &mut AgentHandle) -> AgentEvent {
        loop {
            let event = handle.recv().await.expect("stream ended before a reply");
            if matches!(event, AgentEvent::AssistantMessage { .. }) {
                return event;
            }
        }
    }

    #[tokio::test]
    async fn start_spawns_process_and_streams_events() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::InheritOperatorConfig,
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
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter
            .resume("fixed-session-id", "hello again", &cfg)
            .unwrap();

        let first = handle.recv().await.unwrap();
        let AgentEvent::SessionMeta { session_id, .. } = first else {
            panic!("expected session_meta, got {first:?}");
        };
        assert_eq!(session_id, "fixed-session-id");
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
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        let reply = next_assistant_message(&mut handle).await;
        let AgentEvent::AssistantMessage { text } = reply else {
            panic!("expected an assistant message, got {reply:?}");
        };
        assert!(
            text.starts_with(
                "model=<unset>|system_prompt=<unset>|permission_mode=bypassPermissions"
            ),
            "got {text}"
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
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        let reply = next_assistant_message(&mut handle).await;
        let AgentEvent::AssistantMessage { text } = reply else {
            panic!("expected an assistant message, got {reply:?}");
        };
        assert!(
            text.starts_with("model=<unset>|system_prompt=<unset>|permission_mode=<unset>"),
            "got {text}"
        );
    }

    /// Issue #73: the tool is present on *every* turn, whether or not the
    /// stage routes on it — `--mcp-config` is unconditional. A role that
    /// inherits the operator's setup (#90) doesn't get `--strict-mcp-config`,
    /// so the operator's own MCP servers stay available to it. With no
    /// outcomes, nothing is appended to the system prompt: a standing
    /// session is never told to report.
    #[tokio::test]
    async fn a_turn_with_no_outcomes_still_gets_the_tool_but_no_routing_instruction() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        let reply = next_assistant_message(&mut handle).await;
        let AgentEvent::AssistantMessage { text } = reply else {
            panic!("expected an assistant message, got {reply:?}");
        };
        assert!(
            text.contains("mcp_config=") && !text.contains("mcp_config=<unset>"),
            "got {text}"
        );
        assert!(text.contains("strict_mcp_config=false"), "got {text}");
        assert!(text.contains("append_system_prompt=<unset>"), "got {text}");
    }

    /// The routing half of the same wiring: a stage with `on:` edges gets a
    /// generated system-prompt instruction naming them, built from the exact
    /// same list the tool's own schema uses, so the two can never disagree.
    ///
    /// Parses `mcp_config` and `append_system_prompt` out of the reply
    /// separately and checks each on its own terms (review, #75 round 2):
    /// a loose "does 'approved' appear anywhere in the whole reply" check
    /// could pass even if the tool's own `--outcome` argv were wrong, since
    /// `append_system_prompt`'s prose also names both outcomes.
    #[tokio::test]
    async fn a_turn_with_outcomes_gets_a_routing_instruction_naming_them() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
            report_outcomes: vec!["approved".to_string(), "changes_requested".to_string()],
            report_sections: Vec::new(),
            isolation: Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        let reply = next_assistant_message(&mut handle).await;
        let AgentEvent::AssistantMessage { text } = reply else {
            panic!("expected an assistant message, got {reply:?}");
        };

        // `--outcome` is repeatable (review, #75 round 1's comma-safety
        // fix), so this pins the actual argv shape, not a joined string.
        let mcp_config_json = text
            .split("|mcp_config=")
            .nth(1)
            .and_then(|rest| rest.split("|strict_mcp_config=").next())
            .expect("mcp_config field");
        let mcp_config: Value = serde_json::from_str(mcp_config_json).unwrap();
        let args: Vec<&str> = mcp_config["mcpServers"]["chocofactory"]["args"]
            .as_array()
            .expect("args array")
            .iter()
            .map(|a| a.as_str().expect("string arg"))
            .collect();
        assert_eq!(
            args,
            vec![
                "mcp-serve",
                "--outcome",
                "approved",
                "--outcome",
                "changes_requested"
            ],
            "got {args:?}"
        );
        assert!(text.contains("strict_mcp_config=false"), "got {text}");

        let append_system_prompt = text
            .split("|append_system_prompt=")
            .nth(1)
            .expect("append_system_prompt field");
        assert_ne!(append_system_prompt, "<unset>", "got {text}");
        assert!(append_system_prompt.contains("approved"), "got {text}");
        assert!(
            append_system_prompt.contains("changes_requested"),
            "got {text}"
        );
    }

    /// #95: a stage's `report_sections:` reach the tool as repeated
    /// `--require-section` argv, after the outcomes and in the order the
    /// workflow declared them. Without this, enforcement would be
    /// configured in the workflow and silently absent at the tool, and the
    /// only symptom would be reviewers going on reporting thin.
    #[tokio::test]
    async fn a_turn_with_report_sections_passes_them_to_the_tool() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: false,
            report_outcomes: vec!["approved".to_string()],
            report_sections: vec!["Branches → tests".to_string(), "Findings".to_string()],
            isolation: Isolation::InheritOperatorConfig,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();

        let reply = next_assistant_message(&mut handle).await;
        let AgentEvent::AssistantMessage { text } = reply else {
            panic!("expected an assistant message, got {reply:?}");
        };
        let mcp_config_json = text
            .split("|mcp_config=")
            .nth(1)
            .and_then(|rest| rest.split("|strict_mcp_config=").next())
            .expect("mcp_config field");
        let mcp_config: Value = serde_json::from_str(mcp_config_json).unwrap();
        let args: Vec<&str> = mcp_config["mcpServers"]["chocofactory"]["args"]
            .as_array()
            .expect("args array")
            .iter()
            .map(|a| a.as_str().expect("string arg"))
            .collect();
        assert_eq!(
            args,
            vec![
                "mcp-serve",
                "--outcome",
                "approved",
                "--require-section",
                "Branches → tests",
                "--require-section",
                "Findings",
            ],
            "got {args:?}"
        );
    }

    /// The `key=value` fields of `fake_claude_echo_args.py`'s reply. None of
    /// the values these tests pass contain a `|`.
    fn echo_fields(text: &str) -> HashMap<String, String> {
        text.split('|')
            .filter_map(|field| field.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    async fn echo_args_for(isolation: Isolation) -> HashMap<String, String> {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: true,
            report_outcomes: vec!["done".to_string()],
            report_sections: Vec::new(),
            isolation,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();
        let AgentEvent::AssistantMessage { text } = next_assistant_message(&mut handle).await
        else {
            unreachable!()
        };
        echo_fields(&text)
    }

    /// #90's default: a role that says nothing about isolation gets none of
    /// the operator's settings, plugins, hooks, output style, MCP servers,
    /// skills or memory, and no `ReportFindings`.
    #[tokio::test]
    async fn an_isolated_spawn_drops_the_operators_setup() {
        let fields = echo_args_for(Isolation::default()).await;
        assert_eq!(fields["setting_sources"], "project,local");
        assert_eq!(fields["strict_mcp_config"], "true");
        assert_eq!(fields["disallowed_tools"], "ReportFindings,Skill");
        assert_eq!(fields["disable_auto_memory"], "1");
        let initialize: Value = serde_json::from_str(&fields["initialize"]).unwrap();
        assert_eq!(initialize, json!({ "subtype": "initialize", "skills": [] }));
    }

    #[tokio::test]
    async fn an_isolated_spawn_allows_the_listed_skills_and_memory() {
        let fields = echo_args_for(Isolation::Isolated {
            skills: vec!["run-tests".to_string()],
            memory: true,
        })
        .await;
        assert_eq!(fields["disallowed_tools"], "ReportFindings");
        assert_eq!(fields["disable_auto_memory"], "<unset>");
        let initialize: Value = serde_json::from_str(&fields["initialize"]).unwrap();
        assert_eq!(
            initialize,
            json!({ "subtype": "initialize", "skills": ["run-tests"] })
        );
    }

    #[tokio::test]
    async fn a_spawn_that_inherits_the_operators_setup_gets_no_isolation_flags() {
        let fields = echo_args_for(Isolation::InheritOperatorConfig).await;
        assert_eq!(fields["setting_sources"], "<unset>");
        assert_eq!(fields["strict_mcp_config"], "false");
        assert_eq!(fields["disallowed_tools"], "<unset>");
        assert_eq!(fields["disable_auto_memory"], "<unset>");
        assert_eq!(fields["initialize"], "<unset>");
    }

    /// #90: without `alwaysLoad` the CLI defers `report_outcome` behind
    /// `ToolSearch`, and a turn that can't find its completion call can't
    /// complete. Every spawn sets it, isolated or not.
    #[tokio::test]
    async fn every_spawn_loads_report_outcome_up_front_and_is_told_how_to_complete() {
        for isolation in [Isolation::default(), Isolation::InheritOperatorConfig] {
            let fields = echo_args_for(isolation).await;
            let mcp_config: Value = serde_json::from_str(&fields["mcp_config"]).unwrap();
            assert_eq!(
                mcp_config["mcpServers"]["chocofactory"]["alwaysLoad"], true,
                "got {mcp_config}"
            );
            let instruction = &fields["append_system_prompt"];
            assert!(instruction.contains("done"), "got {instruction}");
            assert!(
                instruction.contains("Calling it is how this stage completes"),
                "got {instruction}"
            );
        }
    }

    /// #90: a sub-agent's lines carry the id of the `Agent` call that spawned
    /// it (as seen on a real 2.1.272 stream) and are wrapped, so they can
    /// never pass for the main agent's.
    #[test]
    fn a_sub_agents_lines_are_wrapped_with_their_parent_tool_use_id() {
        let line = r#"{"type":"assistant","parent_tool_use_id":"toolu_agent","message":{"content":[{"type":"tool_use","id":"toolu_sub","name":"Bash","input":{"command":"echo hi"}},{"type":"text","text":"ran it"}]},"session_id":"s"}"#;
        let mut tool_names = HashMap::new();
        let events = normalize(&parse(line), &mut tool_names);
        assert_eq!(
            events,
            vec![
                AgentEvent::Subagent {
                    parent_tool_use_id: "toolu_agent".to_string(),
                    event: Box::new(AgentEvent::ToolCall {
                        tool_use_id: "toolu_sub".to_string(),
                        tool: "Bash".to_string(),
                        input: json!({ "command": "echo hi" }),
                    }),
                },
                AgentEvent::Subagent {
                    parent_tool_use_id: "toolu_agent".to_string(),
                    event: Box::new(AgentEvent::AssistantMessage {
                        text: "ran it".to_string(),
                    }),
                },
            ]
        );
        let payload = events[1].payload();
        assert_eq!(payload["text"], "ran it");
        assert_eq!(payload["parent_tool_use_id"], "toolu_agent");
    }

    #[test]
    fn a_main_agent_line_with_a_null_parent_is_not_wrapped() {
        let line = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"hi"}]},"session_id":"s"}"#;
        let mut tool_names = HashMap::new();
        assert_eq!(
            normalize(&parse(line), &mut tool_names),
            vec![AgentEvent::AssistantMessage {
                text: "hi".to_string()
            }]
        );
    }

    /// The `init` fields worth keeping, as the real CLI names them, and none
    /// of the rest (the socket path, analytics flags, and so on).
    #[test]
    fn init_keeps_the_sessions_real_environment() {
        let line = r#"{"type":"system","subtype":"init","session_id":"s","claude_code_version":"2.1.272","model":"claude-haiku-4-5","permissionMode":"bypassPermissions","output_style":"default","tools":["Bash","mcp__chocofactory__report_outcome"],"mcp_servers":[{"name":"chocofactory","status":"connected"}],"plugins":[],"skills":["allowed-skill"],"agents":["general-purpose"],"messaging_socket_path":"/tmp/x","analytics_disabled":false}"#;
        let mut tool_names = HashMap::new();
        let events = normalize(&parse(line), &mut tool_names);
        let AgentEvent::SessionMeta { details, .. } = &events[0] else {
            panic!("expected session_meta, got {events:?}");
        };
        assert_eq!(
            details["init"],
            json!({
                "claude_code_version": "2.1.272",
                "model": "claude-haiku-4-5",
                "permissionMode": "bypassPermissions",
                "output_style": "default",
                "tools": ["Bash", "mcp__chocofactory__report_outcome"],
                "mcp_servers": [{ "name": "chocofactory", "status": "connected" }],
                "plugins": [],
                "skills": ["allowed-skill"],
                "agents": ["general-purpose"],
            })
        );
    }

    /// If the CLI refuses the `initialize` carrying the skills allowlist, the
    /// allowlist may not be in force; that must reach the timeline.
    #[test]
    fn a_rejected_initialize_is_an_error_event() {
        let rejected = r#"{"type":"control_response","response":{"subtype":"error","request_id":"chocofactory-initialize","error":"unknown field"}}"#;
        let accepted = r#"{"type":"control_response","response":{"subtype":"success","request_id":"chocofactory-initialize","response":{}}}"#;
        let mut tool_names = HashMap::new();
        assert_eq!(
            normalize(&parse(rejected), &mut tool_names),
            vec![AgentEvent::Error {
                message: "the CLI rejected the session's initialize request: unknown field"
                    .to_string()
            }]
        );
        assert_eq!(normalize(&parse(accepted), &mut tool_names), Vec::new());
    }
}
