//! The `omp` adapter (oh-my-pi, a fork of the Pi coding agent): `cli: omp`.
//!
//! One `omp --mode rpc` process per session, driven over its JSONL RPC
//! protocol. The operator's existing omp login is used as is: this adapter
//! never passes `--profile`, never logs in and never reads or links a
//! credential file. Isolation from the operator's own setup is a per-spawn
//! `--config` overlay file plus flags (see [`overlay`] and [`build_args`]).
//!
//! `report_outcome` is a host tool: omp calls back over stdio, the driver
//! validates the call with the same code `choco mcp-serve` uses
//! (`chocofactory_core::mcp::check_report_call`) and reports the call to the
//! engine as an ordinary `ToolCall` / `ToolResult` pair under the qualified
//! tool name, so the engine has no omp-specific path.

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chocofactory_core::mcp::{
    REPORT_OUTCOME_TOOL_NAME, StageReport, check_report_call, qualified_report_outcome_tool_name,
    tool_definition,
};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::pi_family::{
    ChunkAssembler, MessageNormalizer, Reassembled, SessionStats, TurnMessages,
    parse_message_usage, parse_session_stats, read_lf_line, read_repo_instructions,
    render_instruction_files, stats_delta,
};
use super::{
    AdapterError, AgentAdapter, AgentEvent, AgentHandle, BillingMode, InterruptionEvidence,
    Isolation, RoleConfig, RoleTool, TokenCounts, TurnUsage, UsageCounting, report_instruction,
    usage_limit_text,
};

/// The system prompt an isolated role gets when its definition has none.
/// Omitting `--system-prompt` would let the operator's `SYSTEM.md` load.
const DEFAULT_SYSTEM_PROMPT: &str =
    "You are a coding agent working in this repository. Follow the instructions below.\n";

/// What `--append-system-prompt` holds when there is nothing to put in it.
/// The flag is always passed: it stops `~/.omp/agent/APPEND_SYSTEM.md`
/// from loading.
const NO_INSTRUCTIONS: &str = "No repository instruction files were found.\n";

const THINKING_LEVELS: [&str; 9] = [
    "off", "minimal", "low", "medium", "high", "xhigh", "max", "auto", "inherit",
];

const BASE_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "glob", "grep", "todo"];

const READY_TIMEOUT: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const STATS_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// The reassembly limit assumed when `ready` doesn't advertise one.
const DEFAULT_MAX_REASSEMBLED: u64 = 64 * 1024 * 1024;

/// Providers whose omp login is a subscription.
const SUBSCRIPTION_PROVIDERS: [&str; 6] = [
    "openai-codex",
    "github-copilot",
    "cursor",
    "factory-droid",
    "google-gemini-cli",
    "google-antigravity",
];

/// Wraps `omp --mode rpc` (see the module comment).
pub struct OmpAdapter {
    binary: String,
    state_dir: PathBuf,
}

impl OmpAdapter {
    /// `omp` from `PATH`; sessions and overlay files live under `state_dir`.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self::with_binary("omp", state_dir)
    }

    pub fn with_binary(binary: impl Into<String>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            state_dir: state_dir.into(),
        }
    }

    fn session_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    fn spawn(
        &self,
        cfg: &RoleConfig,
        resume: Option<&str>,
        prompt: &str,
    ) -> Result<AgentHandle, AdapterError> {
        let mut warnings = Vec::new();
        let append = match &cfg.isolation {
            Isolation::Isolated { .. } => {
                let (files, file_warnings) = read_repo_instructions(&cfg.cwd);
                warnings = file_warnings;
                Some(append_block(&render_instruction_files(&files), cfg))
            }
            Isolation::InheritOperatorConfig => {
                (!cfg.report_outcomes.is_empty()).then(|| report_instruction(&cfg.report_outcomes))
            }
        };

        let session_dir = self.session_dir();
        let overlay_dir = self.state_dir.join("overlays");
        for dir in [&session_dir, &overlay_dir] {
            std::fs::create_dir_all(dir).map_err(AdapterError::Spawn)?;
        }
        let overlay_path = overlay_dir.join(format!("{}.yml", uuid::Uuid::new_v4()));
        let yaml = serde_yaml::to_string(&overlay(cfg))
            .map_err(|err| AdapterError::Spawn(std::io::Error::other(err)))?;
        write_private_file(&overlay_path, &yaml).map_err(AdapterError::Spawn)?;
        // From here the overlay is removed whenever this guard drops: on a
        // failed spawn right below, or with the handle once the child is
        // done.
        let guard = OverlayGuard(overlay_path.clone());

        let args = build_args(cfg, &overlay_path, &session_dir, append.as_deref(), resume);
        let mut command = Command::new(&self.binary);
        command
            .current_dir(&cfg.cwd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so cancel can signal the whole tree
            // (same as `claude`).
            .process_group(0)
            .kill_on_drop(true);
        scrub_env(&mut command);
        let mut child: Child = command.spawn().map_err(AdapterError::Spawn)?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<AgentEvent>();

        for message in warnings {
            tracing::warn!("{message}");
            events_tx
                .send(AgentEvent::Error { message })
                .expect("events_rx not yet dropped");
        }
        stdin_tx
            .send(prompt.to_string())
            .expect("stdin_rx not yet dropped");

        tokio::spawn(run_stderr_reader(stderr, events_tx.clone()));
        let driver = Driver::new(
            stdin,
            spawn_frame_reader(stdout),
            events_tx,
            DriverConfig {
                binary: self.binary.clone(),
                stage: StageReport {
                    outcomes: cfg.report_outcomes.clone(),
                    required_sections: cfg.report_sections.clone(),
                },
                isolation: cfg.isolation.describe(),
            },
        );
        tokio::spawn(driver.run(stdin_rx));

        Ok(AgentHandle::with_cleanup(
            child,
            events_rx,
            stdin_tx,
            Box::new(guard),
        ))
    }
}

impl AgentAdapter for OmpAdapter {
    fn name(&self) -> &'static str {
        "omp"
    }

    fn validate_role(&self, role: &str, isolation: &Isolation) -> Result<(), String> {
        let Isolation::Isolated { skills, memory } = isolation else {
            return Ok(());
        };
        if *memory {
            return Err(format!(
                "role '{role}' runs on cli 'omp', which can't use memory: true; remove \
                 memory: true or run the role on cli: claude"
            ));
        }
        // `--skills` takes a comma-separated list of glob patterns.
        if let Some(name) = skills
            .iter()
            .find(|name| name.contains([',', '*', '?', '[', ']', '{', '}']))
        {
            return Err(format!(
                "role '{role}' lists skill '{name}', which omp would read as a pattern; skill \
                 names for omp roles can't contain , * ? [ ] {{ }}"
            ));
        }
        Ok(())
    }

    fn start(&self, prompt: &str, cfg: &RoleConfig) -> Result<AgentHandle, AdapterError> {
        self.spawn(cfg, None, prompt)
    }

    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
    ) -> Result<AgentHandle, AdapterError> {
        self.spawn(cfg, Some(session_id), prompt)
    }
}

// ---------------------------------------------------------------------------
// Spawn pieces
// ---------------------------------------------------------------------------

/// Removes the overlay file when dropped. A failed removal is logged, not an
/// error: the file holds no secrets.
struct OverlayGuard(PathBuf);

impl Drop for OverlayGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            tracing::warn!("couldn't remove omp overlay {}: {err}", self.0.display());
        }
    }
}

fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

/// The environment every omp process the adapter starts gets: no profile or
/// config-dir redirect, no OpenTelemetry settings, and the OTel SDK off.
fn scrub_env(command: &mut Command) {
    for var in [
        "OMP_PROFILE",
        "PI_PROFILE",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
    ] {
        command.env_remove(var);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("OTEL_") {
            command.env_remove(name);
        }
    }
    command.env("OTEL_SDK_DISABLED", "true");
}

/// `--approval-mode`: `yolo` only in a disposable worktree; anywhere else
/// `always-ask`, which with `--no-ui` refuses writes and commands outright.
fn approval_mode(cfg: &RoleConfig) -> &'static str {
    if cfg.sandboxed { "yolo" } else { "always-ask" }
}

/// The settings overlay (`--config`), as data.
///
/// `tools.approval`: `report_outcome` is always allowed. Outside a
/// disposable worktree `bash`, `edit` and `write` are pinned to `deny`: in
/// `always-ask` a user `allow` from the operator's or the repo's own
/// settings would otherwise beat the mode, and a user `deny` can't be
/// overridden.
pub fn overlay(cfg: &RoleConfig) -> Value {
    let mut approval = json!({ REPORT_OUTCOME_TOOL_NAME: "allow" });
    if !cfg.sandboxed {
        for tool in ["bash", "edit", "write"] {
            approval[tool] = json!("deny");
        }
    }
    let common = json!({
        "tools": { "approval": approval },
        "dev": { "autoqa": false },
        "telemetry": { "otlpExportEnabled": false },
    });
    let Isolation::Isolated { .. } = cfg.isolation else {
        return common;
    };
    let mut overlay = json!({
        "disabledProviders": [
            "native", "claude-md", "agents-md", "agents", "codex", "gemini", "opencode",
            "github", "cursor", "windsurf", "cline", "vscode", "mcp-json", "claude-plugins",
            "omp-plugins",
        ],
        "disabledExtensions": ["context-file:user:AGENTS.md", "context-file:user:CLAUDE.md"],
        "skills": {
            "enableClaudeUser": false,
            "enableCodexUser": false,
            "enablePiUser": false,
            "enableAgentsUser": false,
        },
        "mcp": { "enableProjectConfig": false },
        "memory": { "backend": "off" },
        "memories": { "enabled": false },
        "advisor": { "enabled": false },
        "async": { "enabled": false },
        "bash": { "autoBackground": { "enabled": false } },
    });
    for (key, value) in common.as_object().expect("an object") {
        overlay[key] = value.clone();
    }
    overlay
}

/// A model string ending in `:<level>` carries its own thinking level.
fn model_has_thinking_suffix(model: &str) -> bool {
    model
        .rsplit_once(':')
        .is_some_and(|(_, suffix)| THINKING_LEVELS.contains(&suffix))
}

/// Value of `--system-prompt` / `--append-system-prompt`: ends with a
/// newline, because omp reads a single-line value as a file path when that
/// file exists.
fn with_trailing_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

/// `--append-system-prompt` for an isolated role: the repo's instruction
/// files, then the report instruction.
fn append_block(rendered_files: &str, cfg: &RoleConfig) -> String {
    let mut block = rendered_files.to_string();
    if !cfg.report_outcomes.is_empty() {
        block.push_str(&report_instruction(&cfg.report_outcomes));
    }
    if block.is_empty() {
        return NO_INSTRUCTIONS.to_string();
    }
    with_trailing_newline(&block)
}

/// The tools omp may use: never `report_outcome` (omp exits 2 on it; it is a
/// host tool), and minus the role's disallowed tools. `task`, `wait`,
/// `eval`, `web_search`, `lsp` and `python` are deliberately absent.
fn allowed_tools(disallowed: &[RoleTool]) -> Vec<&'static str> {
    BASE_TOOLS
        .iter()
        .copied()
        .filter(|tool| {
            !disallowed.iter().any(|role_tool| match role_tool {
                RoleTool::Edit => *tool == "edit",
                RoleTool::Write => *tool == "write",
                RoleTool::NotebookEdit => false,
            })
        })
        .collect()
}

/// The full argument list for one spawn. `append` is the value for
/// `--append-system-prompt`, when one is passed.
pub fn build_args(
    cfg: &RoleConfig,
    overlay_path: &Path,
    session_dir: &Path,
    append: Option<&str>,
    resume: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|item| item.to_string()));
    push(&["--mode", "rpc", "--no-ui", "--no-lsp", "--no-title"]);
    push(&["--config", &overlay_path.to_string_lossy()]);
    push(&["--session-dir", &session_dir.to_string_lossy()]);
    let isolated = matches!(cfg.isolation, Isolation::Isolated { .. });
    if isolated {
        push(&[
            "--system-prompt",
            &cfg.system_prompt
                .as_deref()
                .map(with_trailing_newline)
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
        ]);
    } else if let Some(system_prompt) = &cfg.system_prompt {
        push(&["--system-prompt", &with_trailing_newline(system_prompt)]);
    }
    if let Some(append) = append {
        push(&["--append-system-prompt", append]);
    }
    if isolated {
        push(&["--no-rules"]);
    }
    push(&["--tools", &allowed_tools(&cfg.disallowed_tools).join(",")]);
    if let Isolation::Isolated { skills, .. } = &cfg.isolation {
        if skills.is_empty() {
            push(&["--no-skills"]);
        } else {
            push(&["--skills", &skills.join(",")]);
        }
    }
    push(&["--approval-mode", approval_mode(cfg)]);
    if !cfg.model.as_deref().is_some_and(model_has_thinking_suffix) {
        push(&["--thinking", "medium"]);
    }
    if let Some(model) = &cfg.model {
        push(&["--model", model]);
    }
    if let Some(session_id) = resume {
        push(&["--resume", session_id]);
    }
    args
}

/// `omp --version`, trimmed. `None` on any failure: the version is a nicety
/// for the timeline, never a reason to fail a turn.
async fn omp_version(binary: &str) -> Option<String> {
    let mut command = Command::new(binary);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    scrub_env(&mut command);
    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// How this session is paid for, from the provider omp reports and the
/// environment. `env` is a lookup so the rule is testable.
///
/// Accepted residual: a stored omp login beats an environment key and the
/// daemon can't see it, so `ApiKey` may be wrong. The cost is unaffected.
pub fn billing_for(provider: &str, env: &dyn Fn(&str) -> Option<String>) -> BillingMode {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    if SUBSCRIPTION_PROVIDERS.contains(&provider) {
        return BillingMode::Subscription;
    }
    if provider == "anthropic" && set("ANTHROPIC_OAUTH_TOKEN") {
        return BillingMode::Subscription;
    }
    let key = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" => "GEMINI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "xai" => "XAI_API_KEY",
        "groq" => "GROQ_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "azure" => "AZURE_OPENAI_API_KEY",
        _ => return BillingMode::Unknown,
    };
    if set(key) {
        BillingMode::ApiKey
    } else {
        BillingMode::Unknown
    }
}

// ---------------------------------------------------------------------------
// Reading omp's output
// ---------------------------------------------------------------------------

async fn run_stderr_reader(stderr: ChildStderr, events_tx: mpsc::UnboundedSender<AgentEvent>) {
    let mut reader = BufReader::new(stderr);
    while let Ok(Some(line)) = read_lf_line(&mut reader).await {
        if line.trim().is_empty() {
            continue;
        }
        if events_tx.send(AgentEvent::Error { message: line }).is_err() {
            return;
        }
    }
}

/// Reads stdout into whole JSON frames. After the protocol-v2 negotiation
/// succeeds, `rpc_chunk` sequences are reassembled. A bad line or chunk
/// sequence is logged and skipped. The channel closes when stdout ends.
fn spawn_frame_reader(stdout: ChildStdout) -> mpsc::UnboundedReceiver<Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut assembler: Option<ChunkAssembler> = None;
        let mut max_reassembled = DEFAULT_MAX_REASSEMBLED;
        loop {
            let line = match read_lf_line(&mut reader).await {
                Ok(Some(line)) => line,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!("omp: reading stdout failed: {err}");
                    return;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(&line) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!("omp: skipping a stdout line that isn't JSON: {err}");
                    continue;
                }
            };
            let frame = match assembler.as_mut() {
                Some(assembler) => match assembler.feed(frame) {
                    Reassembled::Frame(frame) => frame,
                    Reassembled::Pending => continue,
                    Reassembled::Rejected(reason) => {
                        tracing::warn!("omp: skipping a bad rpc_chunk sequence: {reason}");
                        continue;
                    }
                },
                None => frame,
            };
            match frame.get("type").and_then(Value::as_str) {
                Some("ready") => {
                    if let Some(max) = frame
                        .get("maxReassembledFrameBytes")
                        .and_then(Value::as_u64)
                    {
                        max_reassembled = max;
                    }
                }
                // Chunking starts after this response, so switch on right
                // here, before the next line is read.
                Some("response")
                    if frame.get("command").and_then(Value::as_str)
                        == Some("negotiate_protocol")
                        && frame.get("success").and_then(Value::as_bool) == Some(true) =>
                {
                    assembler = Some(ChunkAssembler::new(max_reassembled));
                }
                _ => {}
            }
            if tx.send(frame).is_err() {
                return;
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// The RPC session driver
// ---------------------------------------------------------------------------

struct DriverConfig {
    binary: String,
    stage: StageReport,
    isolation: Value,
}

/// A prompt omp has finished, waiting for its statistics.
struct Completion {
    is_error: bool,
    sent_at: Instant,
    messages: TurnMessages,
}

struct StatsWait {
    id: String,
    deadline: Instant,
    completion: Completion,
}

/// One driver per omp process. It owns stdin and serialises every outbound
/// frame; only it writes events (apart from stderr lines).
struct Driver {
    stdin: Option<ChildStdin>,
    frames: mpsc::UnboundedReceiver<Value>,
    events_tx: mpsc::UnboundedSender<AgentEvent>,
    config: DriverConfig,
    /// Section-rejection count for `report_outcome`; per process.
    thin_reports: u32,
    normalizer: MessageNormalizer,
    messages: TurnMessages,
    /// The usage baseline; `None` is unknown (the next reading becomes it).
    baseline: Option<SessionStats>,
    billing: BillingMode,
    next_request: u64,
    prompts_sent: u32,
    /// Our prompt ids, with when each was sent.
    prompts: HashMap<String, Instant>,
    /// Finished prompts whose session hasn't settled yet.
    awaiting_settle: Vec<Completion>,
    ready_completions: VecDeque<Completion>,
    stats_wait: Option<StatsWait>,
    /// Events produced while a statistics reading is outstanding; flushed
    /// after the `TurnCompleted` they must follow.
    buffered: Vec<AgentEvent>,
    /// The event receiver is gone: nobody is listening any more.
    closed: bool,
}

impl Driver {
    fn new(
        stdin: ChildStdin,
        frames: mpsc::UnboundedReceiver<Value>,
        events_tx: mpsc::UnboundedSender<AgentEvent>,
        config: DriverConfig,
    ) -> Self {
        Self {
            stdin: Some(stdin),
            frames,
            events_tx,
            config,
            thin_reports: 0,
            normalizer: MessageNormalizer::default(),
            messages: TurnMessages::default(),
            baseline: None,
            billing: BillingMode::Unknown,
            next_request: 0,
            prompts_sent: 0,
            prompts: HashMap::new(),
            awaiting_settle: Vec::new(),
            ready_completions: VecDeque::new(),
            stats_wait: None,
            buffered: Vec::new(),
            closed: false,
        }
    }

    async fn run(mut self, mut stdin_rx: mpsc::UnboundedReceiver<String>) {
        let binary = self.config.binary.clone();
        let version = tokio::spawn(async move { omp_version(&binary).await });
        if let Err(message) = self.startup(version).await {
            tracing::warn!("omp session didn't start: {message}");
            self.emit(AgentEvent::Error { message });
            return;
        }
        let mut stdin_open = true;
        while !self.closed {
            let deadline = self.stats_wait.as_ref().map(|wait| wait.deadline);
            tokio::select! {
                frame = self.frames.recv() => match frame {
                    Some(frame) => self.on_frame(frame).await,
                    None => break,
                },
                text = stdin_rx.recv(), if stdin_open => match text {
                    Some(text) => self.send_prompt(&text).await,
                    None => {
                        // End of input: omp exits once it has finished.
                        stdin_open = false;
                        self.stdin = None;
                    }
                },
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    tracing::warn!("omp: get_session_stats timed out");
                    self.finish_stats(None);
                }
            }
        }
        // Stdout ended. Turns omp had already finished still complete, with
        // no statistics; a turn it hadn't finished ends the way a crashed
        // `claude` does, by closing the channel.
        self.finish_pending_at_exit();
    }

    // -- sending ------------------------------------------------------------

    async fn send_frame(&mut self, frame: &Value) -> bool {
        let Some(stdin) = self.stdin.as_mut() else {
            return false;
        };
        let line = format!("{frame}\n");
        match stdin.write_all(line.as_bytes()).await {
            Ok(()) => stdin.flush().await.is_ok(),
            Err(err) => {
                tracing::warn!("omp: writing to stdin failed: {err}");
                self.stdin = None;
                false
            }
        }
    }

    fn request_id(&mut self, prefix: &str) -> String {
        self.next_request += 1;
        format!("{prefix}-{}", self.next_request)
    }

    async fn send_prompt(&mut self, text: &str) {
        let id = self.request_id("prompt");
        let mut frame = json!({ "id": id, "type": "prompt", "message": text });
        if self.prompts_sent > 0 {
            frame["streamingBehavior"] = json!("followUp");
        }
        self.prompts_sent += 1;
        self.prompts.insert(id, Instant::now());
        if !self.send_frame(&frame).await {
            self.emit(AgentEvent::Error {
                message: "omp: couldn't write a prompt to the process".to_string(),
            });
        }
    }

    /// Sends one command and waits for its response, handling any other
    /// frame that arrives meanwhile. Startup only.
    async fn request(&mut self, ty: &str, fields: Value) -> Result<Value, String> {
        let id = self.request_id(ty);
        let mut frame = fields;
        frame["id"] = json!(id);
        frame["type"] = json!(ty);
        if !self.send_frame(&frame).await {
            return Err(format!("couldn't send {ty}: stdin is closed"));
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let frame = match tokio::time::timeout_at(deadline, self.frames.recv()).await {
                Err(_) => return Err(format!("{ty} timed out")),
                Ok(None) => return Err(format!("omp exited before answering {ty}")),
                Ok(Some(frame)) => frame,
            };
            if frame.get("type").and_then(Value::as_str) == Some("response")
                && frame.get("id").and_then(Value::as_str) == Some(id.as_str())
            {
                return response_data(&frame).map_err(|err| format!("{ty} failed: {err}"));
            }
            self.on_frame(frame).await;
        }
    }

    // -- startup ------------------------------------------------------------

    async fn startup(
        &mut self,
        version: tokio::task::JoinHandle<Option<String>>,
    ) -> Result<(), String> {
        let ready = loop {
            let frame = match tokio::time::timeout(READY_TIMEOUT, self.frames.recv()).await {
                Err(_) => return Err("omp didn't send its ready frame".to_string()),
                Ok(None) => return Err("omp exited before it was ready".to_string()),
                Ok(Some(frame)) => frame,
            };
            if frame.get("type").and_then(Value::as_str) == Some("ready") {
                break frame;
            }
            tracing::debug!("omp: frame before ready: {frame}");
        };

        let supports_v2 = ready
            .get("supportedProtocolVersions")
            .and_then(Value::as_array)
            .is_some_and(|versions| versions.iter().any(|v| v.as_u64() == Some(2)));
        if supports_v2 {
            // A failure leaves the session on protocol v1.
            if let Err(err) = self
                .request("negotiate_protocol", json!({ "protocolVersion": 2 }))
                .await
            {
                tracing::warn!("omp: staying on protocol v1: {err}");
            }
        }

        if let Err(err) = self
            .request(
                "set_event_filter",
                json!({ "events": ["message_end"], "messageUpdates": "delta" }),
            )
            .await
        {
            // Unfiltered is noisier, not wrong: other events are ignored.
            tracing::warn!("omp: {err}");
        }

        let tool = tool_definition(&self.config.stage);
        self.request(
            "set_host_tools",
            json!({ "tools": [{
                "name": REPORT_OUTCOME_TOOL_NAME,
                "label": "Report outcome",
                "description": tool["description"],
                "parameters": tool["inputSchema"],
                "loadMode": "essential",
            }] }),
        )
        .await?;

        let state = self.request("get_state", json!({})).await?;
        let session_id = state
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("get_state returned no sessionId")?
            .to_string();
        let provider = state
            .pointer("/model/provider")
            .and_then(Value::as_str)
            .unwrap_or("");
        let model = match state.pointer("/model/id").and_then(Value::as_str) {
            Some(id) if !provider.is_empty() => Value::String(format!("{provider}/{id}")),
            Some(id) => Value::String(id.to_string()),
            None => Value::Null,
        };
        self.billing = billing_for(provider, &|name| std::env::var(name).ok());
        let tools: Vec<Value> = state
            .get("dumpTools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool.get("name").cloned())
                    .collect()
            })
            .unwrap_or_default();
        let omp_version = match version.await {
            Ok(version) => version,
            Err(err) => {
                tracing::warn!("omp --version task failed: {err}");
                None
            }
        };
        self.emit(AgentEvent::SessionMeta {
            adapter_session_id: session_id,
            details: json!({
                "model": model,
                "tools": tools,
                "omp_version": omp_version,
                "isolation": self.config.isolation,
            }),
        });

        // The baseline for per-turn usage. A failed reading leaves it
        // unknown, and the first turn's figures are then unknown too.
        self.baseline = match self.request("get_session_stats", json!({})).await {
            Ok(data) => parse_session_stats(&data),
            Err(err) => {
                tracing::warn!("omp: no usage baseline: {err}");
                None
            }
        };
        Ok(())
    }

    // -- events -------------------------------------------------------------

    /// Delivers an event, or holds it behind the `TurnCompleted` that is
    /// being assembled.
    fn emit(&mut self, event: AgentEvent) {
        if self.stats_wait.is_some() {
            self.buffered.push(event);
        } else {
            self.send_event(event);
        }
    }

    fn send_event(&mut self, event: AgentEvent) {
        if self.events_tx.send(event).is_err() {
            self.closed = true;
        }
    }

    // -- frames -------------------------------------------------------------

    async fn on_frame(&mut self, frame: Value) {
        match frame.get("type").and_then(Value::as_str) {
            Some("message_end") => self.on_message_end(&frame),
            Some("host_tool_call") => self.on_host_tool_call(&frame).await,
            Some("prompt_result") => self.on_prompt_result(&frame),
            Some("session_settled") => {
                let settled = std::mem::take(&mut self.awaiting_settle);
                self.ready_completions.extend(settled);
                self.start_next_completion().await;
            }
            Some("response") => self.on_response(&frame).await,
            Some("host_tool_cancel") => {
                tracing::debug!("omp: host_tool_cancel ignored: {frame}");
            }
            other => tracing::debug!("omp: ignoring frame {other:?}"),
        }
        self.start_next_completion().await;
    }

    fn on_message_end(&mut self, frame: &Value) {
        let Some(message) = frame.get("message") else {
            tracing::warn!("omp: message_end without a message");
            return;
        };
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            self.messages.add(parse_message_usage(message));
        }
        for event in self.normalizer.normalize(message, REPORT_OUTCOME_TOOL_NAME) {
            self.emit(event);
        }
    }

    async fn on_host_tool_call(&mut self, frame: &Value) {
        let (Some(id), Some(tool_call_id), Some(tool_name)) = (
            frame.get("id").and_then(Value::as_str),
            frame.get("toolCallId").and_then(Value::as_str),
            frame.get("toolName").and_then(Value::as_str),
        ) else {
            tracing::warn!("omp: malformed host_tool_call: {frame}");
            return;
        };
        if tool_name != REPORT_OUTCOME_TOOL_NAME {
            tracing::warn!("omp: unknown host tool '{tool_name}'");
            let reply = host_tool_result(id, &format!("unknown host tool '{tool_name}'"), true);
            self.send_frame(&reply).await;
            return;
        }
        let arguments = frame.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let qualified = qualified_report_outcome_tool_name();
        self.emit(AgentEvent::ToolCall {
            tool_use_id: tool_call_id.to_string(),
            tool: qualified.clone(),
            input: arguments.clone(),
        });
        let check = check_report_call(&self.config.stage, &mut self.thin_reports, &arguments);
        let reply = host_tool_result(id, &check.text, check.is_error);
        self.send_frame(&reply).await;
        self.emit(AgentEvent::ToolResult {
            tool_use_id: tool_call_id.to_string(),
            tool: qualified,
            output: check.text,
            is_error: check.is_error,
        });
    }

    fn on_prompt_result(&mut self, frame: &Value) {
        let Some(sent_at) = frame
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.prompts.remove(id))
        else {
            tracing::debug!("omp: prompt_result for a prompt we didn't send: {frame}");
            return;
        };
        let status = frame.get("status").and_then(Value::as_str).unwrap_or("");
        let is_error = status != "completed";
        if status == "error" {
            let error = frame.get("error").cloned().unwrap_or(Value::Null);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("omp reported an error")
                .to_string();
            let event = if error.get("httpStatus").and_then(Value::as_u64) == Some(429) {
                AgentEvent::Interrupted {
                    message,
                    detected_by: InterruptionEvidence::Structured,
                }
            } else if usage_limit_text(&message) {
                AgentEvent::Interrupted {
                    message,
                    detected_by: InterruptionEvidence::MessageText,
                }
            } else {
                AgentEvent::Error { message }
            };
            self.emit(event);
        }
        let completion = Completion {
            is_error,
            sent_at,
            messages: std::mem::take(&mut self.messages),
        };
        // An error completes at once; anything else waits for the session to
        // settle when omp says background work can still wake it.
        if status != "error" && frame.get("sessionSettled").and_then(Value::as_bool) == Some(false)
        {
            self.awaiting_settle.push(completion);
        } else {
            self.ready_completions.push_back(completion);
        }
    }

    /// A response nobody is waiting on in `request`: the statistics reading
    /// of a finishing turn, or a prompt's own response.
    async fn on_response(&mut self, frame: &Value) {
        let id = frame.get("id").and_then(Value::as_str);
        if let Some(wait) = &self.stats_wait
            && id == Some(wait.id.as_str())
        {
            let reading = match response_data(frame) {
                Ok(data) => parse_session_stats(&data).or_else(|| {
                    tracing::warn!("omp: get_session_stats returned an unreadable shape");
                    None
                }),
                Err(err) => {
                    tracing::warn!("omp: get_session_stats failed: {err}");
                    None
                }
            };
            self.finish_stats(reading);
            return;
        }
        if frame.get("success").and_then(Value::as_bool) == Some(false) {
            // A prompt that failed before reaching the agent also gets a
            // `prompt_result`, which is what completes the turn.
            tracing::warn!("omp: a command failed: {frame}");
        }
    }

    // -- completion ---------------------------------------------------------

    async fn start_next_completion(&mut self) {
        if self.stats_wait.is_some() {
            return;
        }
        let Some(completion) = self.ready_completions.pop_front() else {
            return;
        };
        let id = self.request_id("stats");
        let frame = json!({ "id": id, "type": "get_session_stats" });
        if !self.send_frame(&frame).await {
            self.complete(completion, None);
            return;
        }
        self.stats_wait = Some(StatsWait {
            id,
            deadline: Instant::now() + STATS_TIMEOUT,
            completion,
        });
    }

    /// Ends the outstanding statistics wait with `reading` (`None` = failed).
    fn finish_stats(&mut self, reading: Option<SessionStats>) {
        let Some(wait) = self.stats_wait.take() else {
            return;
        };
        self.complete(wait.completion, reading);
        // Everything that arrived while waiting follows the TurnCompleted.
        for event in std::mem::take(&mut self.buffered) {
            self.send_event(event);
        }
    }

    fn complete(&mut self, completion: Completion, reading: Option<SessionStats>) {
        let (tokens, stats_cost) = match (&self.baseline, &reading) {
            (Some(before), Some(now)) => stats_delta(before, now),
            _ => (TokenCounts::default(), None),
        };
        // A failed reading invalidates the baseline; the next good one
        // becomes the new one.
        self.baseline = reading;
        let messages = &completion.messages;
        let usage = TurnUsage {
            cost_usd: if messages.all_unpriced() {
                None
            } else {
                stats_cost
            },
            tokens,
            models: messages.models(),
            wall_time_ms: Some(
                u64::try_from(completion.sent_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            ),
            model_turns: Some(messages.assistant_messages),
            billing: self.billing,
            counting: UsageCounting::PerTurn,
        };
        self.send_event(AgentEvent::TurnCompleted {
            is_error: completion.is_error,
            usage,
        });
    }

    fn finish_pending_at_exit(&mut self) {
        if self.stats_wait.is_some() {
            self.finish_stats(None);
        }
        let pending: Vec<Completion> = std::mem::take(&mut self.awaiting_settle)
            .into_iter()
            .chain(std::mem::take(&mut self.ready_completions))
            .collect();
        for completion in pending {
            self.complete(completion, None);
        }
        for event in std::mem::take(&mut self.buffered) {
            self.send_event(event);
        }
    }
}

/// `data` of a successful response, or its error text.
fn response_data(frame: &Value) -> Result<Value, String> {
    if frame.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(frame.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(frame
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("no error text")
            .to_string())
    }
}

/// The reply to a `host_tool_call`; the top-level `isError` is set only
/// when the call is rejected.
fn host_tool_result(id: &str, text: &str, is_error: bool) -> Value {
    let mut reply = json!({
        "type": "host_tool_result",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": is_error },
    });
    if is_error {
        reply["isError"] = json!(true);
    }
    reply
}

#[cfg(test)]
#[path = "omp_tests.rs"]
mod tests;
