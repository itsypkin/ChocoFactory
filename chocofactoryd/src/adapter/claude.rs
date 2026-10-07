use std::collections::HashMap;
use std::process::Stdio;

use chocofactory_core::mcp::MCP_SERVER_NAME;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{
    AdapterError, AgentAdapter, AgentEvent, AgentHandle, BillingMode, InterruptionEvidence,
    Isolation, ModelUsage, RoleConfig, RoleTool, TokenCounts, TurnUsage, UsageCounting,
    report_instruction, usage_limit_text,
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

    /// The `choco` binary path embedded in every agent turn's `--mcp-config`.
    pub fn choco_binary(&self) -> &str {
        &self.choco_binary
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
    fn name(&self) -> &'static str {
        "claude"
    }

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

/// #115: tools that schedule or wait for something later. They do work under
/// `--print` when stdin stays open: a wake-up, a monitor or a cron schedule
/// starts a new turn. The problem is who owns that turn. A workflow agent
/// that ends its turn to wait on its own timer races the daemon's nudge
/// clock (`nudge_after`, `max_nudges`, then the turn closes as `no_report`)
/// and spends wall-clock time the daemon can't see, so workflow roles lose
/// all of them.
/// - `ScheduleWakeup`, `Monitor`: the agent waits on its own timer or
///   watcher instead of the daemon's.
/// - `CronCreate`, `CronDelete`, `CronList`: recurring turns the task
///   doesn't own.
/// - `RemoteTrigger`: schedules cloud agents that would run outside choco,
///   after the turn, as the operator.
///
/// Background `Bash` and `Agent` stay available: the workflows rely on them
/// waking the turn correctly. An unknown name in `--disallowedTools` is
/// ignored, so naming a tool an older CLI lacks is safe.
const TIMER_TOOLS: [&str; 6] = [
    "ScheduleWakeup",
    "Monitor",
    "CronCreate",
    "CronDelete",
    "CronList",
    "RemoteTrigger",
];

/// #115: the subset of `TIMER_TOOLS` the chat role loses. Chat is a standing
/// session that is never nudged, so a one-off wait the operator asked for
/// ("check CI in 10 minutes") works there: `ScheduleWakeup` and `Monitor`
/// stay. Cron and remote triggers start unattended or recurring turns, and
/// may keep the session from being idle-reaped.
const CHAT_BLOCKED_TOOLS: [&str; 4] = ["CronCreate", "CronDelete", "CronList", "RemoteTrigger"];

/// Appends the role's neutral tool names (#172), mapped to `claude`'s own,
/// after the adapter's entries, skipping any already present.
fn merge_role_tools(disallowed: &mut Vec<&str>, tools: &[RoleTool]) {
    for tool in tools {
        let name = match tool {
            RoleTool::Edit => "Edit",
            RoleTool::Write => "Write",
            RoleTool::NotebookEdit => "NotebookEdit",
        };
        if !disallowed.contains(&name) {
            disallowed.push(name);
        }
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
        // agent. The daemon now shuts down gracefully (SIGTERM/Ctrl-C
        // stop every live group via `SessionManager::shutdown`), so this
        // only matters for SIGKILL, where `kill_on_drop` cannot run; the
        // startup park sweep covers that case by parking the interrupted
        // task.
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
    // piece was checked against a real session (Claude Code 2.1.291). The
    // rule (#183): an isolated role reads the task repo's own `CLAUDE.md`
    // and `AGENTS.md`, and nothing personal or from above the repo.
    //
    // - `--setting-sources project` skips user and local *settings*
    //   (plugins, hooks, output style). `.claude/settings.local.json` is the
    //   operator's personal, uncommitted file, and in a linked worktree
    //   (every choco task) Claude Code resolves it to the main checkout's
    //   (#141). It does NOT stop `~/.claude/CLAUDE.md` or the `CLAUDE.md`
    //   files in folders above the repo from loading. The task repo's
    //   committed `.claude/settings.json` still applies.
    // - `--strict-mcp-config` drops the operator's MCP servers, leaving only
    //   ours.
    // - `--settings` (`isolated_settings`) scopes the instruction files: its
    //   `claudeMdExcludes` negated glob excludes every `CLAUDE.md` outside
    //   the canonical working directory, the personal one included. Its
    //   `pluginConfigs` switches on `AGENTS.md` loading in the built-in
    //   agents-md plugin, which is otherwise off. The built-in plugins
    //   (`cc-plugin-agents-md`, `cc-plugin-telemetry`,
    //   `cc-plugin-plugin-authoring`) do appear on an isolated role's `init`
    //   line; `agents_md_plugin_warning` checks the first is there.
    // - `ReportFindings` is a built-in verdict tool that reviewers reached
    //   for instead of `report_outcome` (#61).
    // - With no skills allowed, the `Skill` tool is removed outright; with
    //   some, the allowlist is sent on stdin below.
    // - Auto-memory (`CLAUDE_CODE_DISABLE_AUTO_MEMORY`) is decided by
    //   `apply_auto_memory_env` below, not read from the daemon's own
    //   environment (#105).
    apply_auto_memory_env(&mut command, &cfg.isolation);
    //
    // #115: workflow (isolated) roles lose all of `TIMER_TOOLS`; chat loses
    // only `CHAT_BLOCKED_TOOLS` (see their comments for why). "Inherit the
    // operator's config" means the operator's settings, plugins and MCP
    // servers, not tools that start turns nobody supervises.
    let mut spawn_warnings: Vec<String> = Vec::new();
    let initialize = match &cfg.isolation {
        Isolation::InheritOperatorConfig => {
            let mut disallowed: Vec<&str> = CHAT_BLOCKED_TOOLS.to_vec();
            merge_role_tools(&mut disallowed, &cfg.disallowed_tools);
            command.arg("--disallowedTools").arg(disallowed.join(","));
            None
        }
        Isolation::Isolated { skills, memory: _ } => {
            let (settings, warnings) = isolated_settings(&cfg.cwd);
            spawn_warnings = warnings;
            command
                .arg("--setting-sources")
                .arg("project")
                .arg("--strict-mcp-config")
                .arg("--settings")
                .arg(settings);
            let mut disallowed = vec!["ReportFindings"];
            disallowed.extend(TIMER_TOOLS);
            if skills.is_empty() {
                disallowed.push("Skill");
            }
            merge_role_tools(&mut disallowed, &cfg.disallowed_tools);
            command.arg("--disallowedTools").arg(disallowed.join(","));
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

    // A fallback while building the settings is never silent: it goes to the
    // daemon log and to the timeline, and the spawn carries on.
    for message in spawn_warnings {
        tracing::warn!("{message}");
        events_tx
            .send(AgentEvent::Error { message })
            .expect("events_rx not yet dropped");
    }

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
        cfg.isolation.clone(),
    ));

    Ok(AgentHandle::new(child, events_rx, stdin_tx))
}

/// The single place `CLAUDE_CODE_DISABLE_AUTO_MEMORY` is decided (#105).
///
/// A role's auto-memory setting must depend only on its workflow
/// definition, never on whatever the daemon's own environment happens to
/// hold. In practice a value already in that environment is almost always
/// an artifact of *where the daemon was launched* — inside an isolated
/// agent, which sets this same variable for itself — not a choice the
/// operator made for this particular role. So every branch states its own
/// answer explicitly instead of leaving the variable to inherit:
///
/// - `Isolated { memory: false, .. }`: set to `"1"`.
/// - `Isolated { memory: true, .. }`: removed, so a role whose definition
///   asks for memory gets it even if the daemon's own environment disables
///   it.
/// - `InheritOperatorConfig`: also removed, for the same reason — the
///   role's definition is the only source of truth for its memory, and
///   "inherit the operator's config" describes the CLI flags this turn
///   gets, not license to leak the daemon's launch environment into it.
fn apply_auto_memory_env(command: &mut Command, isolation: &Isolation) {
    match isolation {
        Isolation::Isolated { memory: false, .. } => {
            command.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
        }
        Isolation::Isolated { memory: true, .. } | Isolation::InheritOperatorConfig => {
            command.env_remove("CLAUDE_CODE_DISABLE_AUTO_MEMORY");
        }
    }
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
    role_isolation: Isolation,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut billing = BillingMode::Unknown;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for mut event in normalize(&value, &mut tool_names, &mut billing) {
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
        let top_level_init = value.get("type").and_then(Value::as_str) == Some("system")
            && value.get("subtype").and_then(Value::as_str) == Some("init")
            && value.get("parent_tool_use_id").is_none_or(Value::is_null);
        if top_level_init && let Some(event) = agents_md_plugin_warning(&value, &role_isolation) {
            if let AgentEvent::Error { message } = &event {
                let session_id = value.get("session_id").and_then(Value::as_str);
                tracing::warn!(session_id, "{message}");
            }
            if events_tx.send(event).is_err() {
                return;
            }
        }
    }
}

/// Makes a path match itself literally inside a glob.
///
/// Backslash escapes are NOT honoured by Claude Code's `claudeMdExcludes`
/// matcher (probed on 2.1.292: `\(1\)` and `\[x\]` made the repo's own
/// files stop loading), but a one-character class is: `[(]` matches `(`.
/// So each character picomatch treats as syntax (`* ? [ ] { } ( ) + @ | "`) is
/// wrapped in `[` `]`. `!` and `\` can't be put in a class (`[!]` is a
/// negated class, and a backslash inside one is itself an escape), so each
/// becomes `?`, which matches that one character and nothing longer.
fn glob_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '+' | '@' | '|' | '"' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            '!' | '\\' => out.push('?'),
            _ => out.push(c),
        }
    }
    out
}

/// The `--settings` JSON for an isolated role (#183) and any warnings about
/// falling back while building it. Only instruction files under the role's
/// canonical working directory load, and the built-in agents-md plugin is
/// told to read `AGENTS.md` as well as `CLAUDE.md`.
fn isolated_settings(cwd: &std::path::Path) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let resolved = match std::fs::canonicalize(cwd) {
        Ok(path) => path,
        Err(err) => {
            warnings.push(format!(
                "could not resolve the real path of {} ({err}); instruction files are scoped to it as given",
                cwd.display()
            ));
            cwd.to_path_buf()
        }
    };
    let path = utf8_path(&resolved, cwd, &mut warnings);
    let escaped = glob_escape(&path);
    let settings = json!({
        "claudeMdExcludes": [format!("!{}/**", escaped.trim_end_matches('/'))],
        "pluginConfigs": {
            "cc-plugin-agents-md@builtin": {
                "options": { "instructionFiles": "claude-md-and-agents-md" }
            }
        }
    });
    (settings.to_string(), warnings)
}

/// `resolved` as a string, lossily (with a warning) when it isn't UTF-8.
fn utf8_path(
    resolved: &std::path::Path,
    cwd: &std::path::Path,
    warnings: &mut Vec<String>,
) -> String {
    match resolved.to_str() {
        Some(path) => path.to_string(),
        None => {
            warnings.push(format!(
                "the working directory {} is not valid UTF-8; instruction-file scoping may exclude the repo's own CLAUDE.md/AGENTS.md",
                cwd.display()
            ));
            resolved.to_string_lossy().into_owned()
        }
    }
}

/// A warning when an isolated role's `init` line lists plugins and none is
/// the built-in agents-md plugin: the plugin sits behind a server-side
/// feature flag, and without it `AGENTS.md` silently stops loading.
fn agents_md_plugin_warning(init: &Value, isolation: &Isolation) -> Option<AgentEvent> {
    if matches!(isolation, Isolation::InheritOperatorConfig) {
        return None;
    }
    let plugins = init.get("plugins")?.as_array()?;
    const NAMES: [&str; 2] = ["cc-plugin-agents-md", "agents-md"];
    const SOURCES: [&str; 2] = ["cc-plugin-agents-md@builtin", "agents-md@builtin"];
    let is_agents_md = |entry: &Value| match entry {
        Value::String(s) => NAMES.contains(&s.as_str()) || SOURCES.contains(&s.as_str()),
        Value::Object(o) => {
            o.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| NAMES.contains(&n))
                || o.get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|n| SOURCES.contains(&n))
        }
        _ => false,
    };
    if plugins.iter().any(is_agents_md) {
        return None;
    }
    Some(AgentEvent::Error {
        message: "Claude Code's built-in agents-md plugin (cc-plugin-agents-md@builtin) is not among this session's plugins, so the repo's AGENTS.md files won't be loaded".to_string(),
    })
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
fn normalize(
    value: &Value,
    tool_names: &mut HashMap<String, String>,
    billing: &mut BillingMode,
) -> Vec<AgentEvent> {
    let events = match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            if value.get("parent_tool_use_id").is_none_or(Value::is_null) {
                *billing = match value.get("apiKeySource").and_then(Value::as_str) {
                    Some("none") => BillingMode::Subscription,
                    Some(_) => BillingMode::ApiKey,
                    None => BillingMode::Unknown,
                };
            }
            let adapter_session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            vec![AgentEvent::SessionMeta {
                adapter_session_id,
                details: json!({ "init": init_summary(value) }),
            }]
        }
        Some("assistant") => normalize_assistant(value, tool_names),
        Some("user") => normalize_user(value, tool_names),
        Some("result") => normalize_result(value, *billing),
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
    const KEYS: [&str; 10] = [
        "claude_code_version",
        "model",
        "permissionMode",
        "output_style",
        "tools",
        "mcp_servers",
        "plugins",
        "skills",
        "agents",
        "apiKeySource",
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
fn normalize_result(value: &Value, billing: BillingMode) -> Vec<AgentEvent> {
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_error {
        return vec![AgentEvent::TurnCompleted {
            is_error: false,
            usage: result_usage(value, billing),
        }];
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
    vec![
        first,
        AgentEvent::TurnCompleted {
            is_error: true,
            usage: result_usage(value, billing),
        },
    ]
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    let n = value.get(key)?;
    n.as_u64().or_else(|| {
        n.as_f64()
            .filter(|f| *f >= 0.0 && f.fract() == 0.0)
            .map(|f| f as u64)
    })
}

/// The usage fields of a `result` line. Each missing or non-numeric field
/// is `None` on its own; nothing here can fail the turn.
fn result_usage(value: &Value, billing: BillingMode) -> TurnUsage {
    let tokens = match value.get("usage") {
        Some(usage) if usage.is_object() => TokenCounts {
            input: u64_field(usage, "input_tokens"),
            output: u64_field(usage, "output_tokens"),
            cache_read: u64_field(usage, "cache_read_input_tokens"),
            cache_write: u64_field(usage, "cache_creation_input_tokens"),
        },
        _ => TokenCounts {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
        },
    };
    let models = value
        .get("modelUsage")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(model, m)| ModelUsage {
                    model: model.clone(),
                    tokens: TokenCounts {
                        input: u64_field(m, "inputTokens"),
                        output: u64_field(m, "outputTokens"),
                        cache_read: u64_field(m, "cacheReadInputTokens"),
                        cache_write: u64_field(m, "cacheCreationInputTokens"),
                    },
                    cost_usd: m.get("costUSD").and_then(Value::as_f64),
                })
                .collect()
        });
    TurnUsage {
        cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
        tokens,
        models,
        wall_time_ms: u64_field(value, "duration_ms"),
        model_turns: u64_field(value, "num_turns").and_then(|n| u32::try_from(n).ok()),
        billing,
        counting: UsageCounting::CumulativePerConversation,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chocofactory_core::models::EventType;

    /// The usage of a `result` line that carries none of the usage fields,
    /// with no `init` seen before it.
    fn unknown_usage() -> TurnUsage {
        TurnUsage {
            cost_usd: None,
            tokens: TokenCounts {
                input: None,
                output: None,
                cache_read: None,
                cache_write: None,
            },
            models: None,
            wall_time_ms: None,
            model_turns: None,
            billing: BillingMode::Unknown,
            counting: UsageCounting::CumulativePerConversation,
        }
    }

    const FULL_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","duration_ms":1234,"num_turns":3,"total_cost_usd":0.02927,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20},"modelUsage":{"claude-sonnet-5":{"inputTokens":20,"outputTokens":9,"cacheReadInputTokens":200,"cacheCreationInputTokens":40,"costUSD":0.02927,"costBasis":"list"}},"session_id":"s"}"#;

    fn usage_of(events: Vec<AgentEvent>) -> TurnUsage {
        match events.last() {
            Some(AgentEvent::TurnCompleted { usage, .. }) => usage.clone(),
            other => panic!("expected TurnCompleted last, got {other:?}"),
        }
    }

    #[test]
    fn a_full_result_line_yields_full_usage() {
        let usage = usage_of(normalize(
            &parse(FULL_RESULT),
            &mut HashMap::new(),
            &mut BillingMode::Unknown,
        ));
        assert_eq!(usage.cost_usd, Some(0.02927));
        assert_eq!(
            usage.tokens,
            TokenCounts {
                input: Some(10),
                output: Some(5),
                cache_read: Some(100),
                cache_write: Some(20)
            }
        );
        assert_eq!(
            usage.models,
            Some(vec![ModelUsage {
                model: "claude-sonnet-5".to_string(),
                tokens: TokenCounts {
                    input: Some(20),
                    output: Some(9),
                    cache_read: Some(200),
                    cache_write: Some(40)
                },
                cost_usd: Some(0.02927),
            }])
        );
        assert_eq!(usage.wall_time_ms, Some(1234));
        assert_eq!(usage.model_turns, Some(3));
        assert_eq!(usage.counting, UsageCounting::CumulativePerConversation);
    }

    #[test]
    fn missing_result_fields_are_none_each_on_their_own() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","num_turns":1,"usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}"#;
        let events = normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown);
        assert_eq!(events.len(), 1);
        let usage = usage_of(events);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.tokens.input, Some(10));
        assert_eq!(usage.tokens.output, None);
        assert_eq!(usage.tokens.cache_read, Some(100));
        assert_eq!(usage.models, None);
        assert_eq!(usage.wall_time_ms, None);
        assert_eq!(usage.model_turns, Some(1));
    }

    #[test]
    fn an_error_result_with_missing_usage_still_emits_error_then_turn_completed() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#;
        let events = normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown);
        assert_eq!(
            events,
            vec![
                AgentEvent::Error {
                    message: "boom".to_string()
                },
                AgentEvent::TurnCompleted {
                    is_error: true,
                    usage: unknown_usage()
                },
            ]
        );
    }

    #[test]
    fn a_non_object_usage_gives_no_token_counts() {
        let line = r#"{"type":"result","is_error":false,"usage":"lots","total_cost_usd":0.5}"#;
        let usage = usage_of(normalize(
            &parse(line),
            &mut HashMap::new(),
            &mut BillingMode::Unknown,
        ));
        assert_eq!(usage.tokens, unknown_usage().tokens);
        assert_eq!(usage.cost_usd, Some(0.5));
    }

    #[test]
    fn billing_follows_the_latest_top_level_init() {
        let result = r#"{"type":"result","is_error":false}"#;
        let run = |init: Option<&str>| {
            let mut billing = BillingMode::Unknown;
            let mut names = HashMap::new();
            if let Some(init) = init {
                normalize(&parse(init), &mut names, &mut billing);
            }
            usage_of(normalize(&parse(result), &mut names, &mut billing)).billing
        };
        assert_eq!(
            run(Some(
                r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"none"}"#
            )),
            BillingMode::Subscription
        );
        assert_eq!(
            run(Some(
                r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"ANTHROPIC_API_KEY"}"#
            )),
            BillingMode::ApiKey
        );
        assert_eq!(
            run(Some(
                r#"{"type":"system","subtype":"init","session_id":"s"}"#
            )),
            BillingMode::Unknown
        );
        assert_eq!(
            run(Some(
                r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":7}"#
            )),
            BillingMode::Unknown
        );
        assert_eq!(run(None), BillingMode::Unknown);
    }

    #[test]
    fn a_sub_agent_init_does_not_change_billing() {
        let mut billing = BillingMode::Unknown;
        let mut names = HashMap::new();
        normalize(
            &parse(r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"none"}"#),
            &mut names,
            &mut billing,
        );
        normalize(
            &parse(
                r#"{"type":"system","subtype":"init","session_id":"s","apiKeySource":"KEY","parent_tool_use_id":"toolu_1"}"#,
            ),
            &mut names,
            &mut billing,
        );
        assert_eq!(billing, BillingMode::Subscription);
    }

    #[test]
    fn init_summary_keeps_api_key_source() {
        let summary = init_summary(&parse(
            r#"{"type":"system","subtype":"init","apiKeySource":"none","model":"m"}"#,
        ));
        assert_eq!(summary["apiKeySource"], "none");
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    // Fixtures below are real `claude --print --output-format=stream-json
    // --verbose` output, captured by hand while building this adapter.

    #[test]
    fn normalizes_system_init_to_session_meta() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"9bf8db32-b723-41f6-8963-ea3ece07cb1a","tools":["Bash"],"model":"claude-sonnet-5"}"#;
        let mut tool_names = HashMap::new();
        let events = normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown);
        assert_eq!(
            events,
            vec![AgentEvent::SessionMeta {
                adapter_session_id: "9bf8db32-b723-41f6-8963-ea3ece07cb1a".to_string(),
                details: json!({ "init": { "tools": ["Bash"], "model": "claude-sonnet-5" } }),
            }]
        );
    }

    #[test]
    fn normalizes_assistant_text_block() {
        let line = r#"{"type":"assistant","message":{"model":"claude-sonnet-5","id":"msg_01FpcJagDvX9Hv2LF9yLsdF7","type":"message","role":"assistant","content":[{"type":"text","text":"pong"}],"stop_reason":null},"session_id":"9bf8db32-b723-41f6-8963-ea3ece07cb1a"}"#;
        let mut tool_names = HashMap::new();
        let events = normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown);
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
        let call_events = normalize(
            &parse(tool_use_line),
            &mut tool_names,
            &mut BillingMode::Unknown,
        );
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

        let result_events = normalize(
            &parse(tool_result_line),
            &mut tool_names,
            &mut BillingMode::Unknown,
        );
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
            normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown),
            vec![AgentEvent::TurnCompleted {
                is_error: false,
                usage: unknown_usage()
            }]
        );
    }

    #[test]
    fn normalizes_error_result_to_error_event_then_turn_completed() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","session_id":"abc"}"#;
        let mut tool_names = HashMap::new();
        assert_eq!(
            normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown),
            vec![
                AgentEvent::Error {
                    message: "boom".to_string()
                },
                AgentEvent::TurnCompleted {
                    is_error: true,
                    usage: unknown_usage()
                },
            ]
        );
    }

    #[test]
    fn an_assistant_line_flagged_rate_limit_is_an_interruption() {
        // The shape the CLI recorded for #88's interrupted turn: the limit's
        // own text in the message, and `error`/`apiErrorStatus` beside it.
        let line = r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 3:40pm (Europe/Berlin)"}]},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"session_id":"s"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown),
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
            normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown),
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
            normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown),
            vec![AgentEvent::AssistantMessage {
                text: "working on it".to_string()
            }]
        );
    }

    #[test]
    fn a_rejected_rate_limit_event_is_an_interruption() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1789652400},"session_id":"abc"}"#;
        assert_eq!(
            normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown),
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
            normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown),
            vec![
                AgentEvent::Interrupted {
                    message: "You've hit your session limit · resets 3:40pm (Europe/Berlin)"
                        .to_string(),
                    detected_by: InterruptionEvidence::MessageText,
                },
                AgentEvent::TurnCompleted {
                    is_error: true,
                    usage: unknown_usage()
                },
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
            normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown),
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
        assert_eq!(
            normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown),
            Vec::new()
        );
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
            disallowed_tools: Vec::new(),
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
        assert_eq!(
            third,
            AgentEvent::TurnCompleted {
                is_error: false,
                usage: unknown_usage()
            }
        );

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
            disallowed_tools: Vec::new(),
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
        let AgentEvent::SessionMeta {
            adapter_session_id, ..
        } = first
        else {
            panic!("expected session_meta, got {first:?}");
        };
        assert_eq!(adapter_session_id, "fixed-session-id");
    }

    /// #67: `claude`'s normal permission model expects a human to approve
    /// each edit interactively, which can never happen on the other end of
    /// this stdin pipe — a sandboxed spawn (`cfg.sandboxed`, a disposable
    /// worktree the workflow opted into, §5.5 Q7/#58) opts out of it.
    #[tokio::test]
    async fn a_sandboxed_spawn_bypasses_claudes_own_permission_prompts() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            disallowed_tools: Vec::new(),
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
            disallowed_tools: Vec::new(),
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
            disallowed_tools: Vec::new(),
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
            disallowed_tools: Vec::new(),
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
            disallowed_tools: Vec::new(),
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
        echo_args_for_tools(isolation, Vec::new()).await
    }

    async fn echo_args_for_tools(
        isolation: Isolation,
        disallowed_tools: Vec<RoleTool>,
    ) -> HashMap<String, String> {
        echo_args_in(isolation, disallowed_tools, std::env::temp_dir()).await
    }

    async fn echo_args_in(
        isolation: Isolation,
        disallowed_tools: Vec<RoleTool>,
        cwd: std::path::PathBuf,
    ) -> HashMap<String, String> {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            disallowed_tools,
            cwd,
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

    /// `Command::get_envs` reports `Some(Some(value))` for a variable that
    /// was set, `Some(None)` for one explicitly *removed*, and nothing at
    /// all for one left untouched (inherited from whatever process spawns
    /// the command). That third case is exactly the bug in #105: the old
    /// code left `CLAUDE_CODE_DISABLE_AUTO_MEMORY` untouched for the two
    /// rows below, silently inheriting the daemon's own environment instead
    /// of stating the role's own answer. No process is spawned by these
    /// tests, so they're immune to whatever the shell running `cargo test`
    /// happens to have set.
    fn auto_memory_env(isolation: &Isolation) -> Option<Option<String>> {
        let mut command = Command::new("unused");
        apply_auto_memory_env(&mut command, isolation);
        command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "CLAUDE_CODE_DISABLE_AUTO_MEMORY")
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn isolated_without_memory_disables_auto_memory() {
        let isolation = Isolation::Isolated {
            skills: Vec::new(),
            memory: false,
        };
        assert_eq!(auto_memory_env(&isolation), Some(Some("1".to_string())));
    }

    /// The regression guard for #105: a role whose definition asks for
    /// memory must get it back even when the daemon's own environment
    /// disables it, which requires an explicit removal — not merely leaving
    /// the variable alone — to override whatever the daemon inherited.
    #[test]
    fn isolated_with_memory_removes_the_disable_flag() {
        let isolation = Isolation::Isolated {
            skills: Vec::new(),
            memory: true,
        };
        assert_eq!(auto_memory_env(&isolation), Some(None));
    }

    /// Same regression guard as above, for the other role shape that must
    /// never inherit the daemon's environment (#105): "inherit the
    /// operator's config" describes this turn's CLI flags, not license to
    /// leak the daemon's own launch environment into it.
    #[test]
    fn inherit_operator_config_removes_the_disable_flag() {
        assert_eq!(
            auto_memory_env(&Isolation::InheritOperatorConfig),
            Some(None)
        );
    }

    /// #90's default: a role that says nothing about isolation gets none of
    /// the operator's settings, plugins, hooks, output style, MCP servers,
    /// skills or memory, and no `ReportFindings`.
    /// Only `project` settings: `local` is the operator's file (#141).
    /// #172: a role's neutral tool names are mapped and appended after the
    /// adapter's own entries, for isolated and inheriting roles alike.
    #[tokio::test]
    async fn a_roles_disallowed_tools_are_appended_to_the_denylist() {
        let fields = echo_args_for_tools(Isolation::default(), RoleTool::ALL.to_vec()).await;
        assert_eq!(
            fields["disallowed_tools"],
            "ReportFindings,ScheduleWakeup,Monitor,CronCreate,CronDelete,CronList,RemoteTrigger,Skill,Edit,Write,NotebookEdit"
        );
        let fields =
            echo_args_for_tools(Isolation::InheritOperatorConfig, RoleTool::ALL.to_vec()).await;
        assert_eq!(
            fields["disallowed_tools"],
            "CronCreate,CronDelete,CronList,RemoteTrigger,Edit,Write,NotebookEdit"
        );
        let fields = echo_args_for(Isolation::InheritOperatorConfig).await;
        assert_eq!(
            fields["disallowed_tools"],
            "CronCreate,CronDelete,CronList,RemoteTrigger"
        );
    }

    #[tokio::test]
    async fn an_isolated_spawn_drops_the_operators_setup() {
        let fields = echo_args_for(Isolation::default()).await;
        assert_eq!(fields["setting_sources"], "project");
        assert_eq!(fields["strict_mcp_config"], "true");
        assert_eq!(
            fields["disallowed_tools"],
            "ReportFindings,ScheduleWakeup,Monitor,CronCreate,CronDelete,CronList,RemoteTrigger,Skill"
        );
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
        assert_eq!(
            fields["disallowed_tools"],
            "ReportFindings,ScheduleWakeup,Monitor,CronCreate,CronDelete,CronList,RemoteTrigger"
        );
        assert_eq!(fields["disable_auto_memory"], "<unset>");
        let initialize: Value = serde_json::from_str(&fields["initialize"]).unwrap();
        assert_eq!(
            initialize,
            json!({ "subtype": "initialize", "skills": ["run-tests"] })
        );
    }

    #[tokio::test]
    async fn a_spawn_that_inherits_the_operators_setup_gets_no_isolation_flags_but_loses_timer_tools()
     {
        let fields = echo_args_for(Isolation::InheritOperatorConfig).await;
        assert_eq!(fields["setting_sources"], "<unset>");
        assert_eq!(fields["strict_mcp_config"], "false");
        // #115: a decision, not an isolation flag. Chat keeps `ReportFindings`,
        // `Skill`, `ScheduleWakeup` and `Monitor`; it loses only the cron and
        // remote-trigger tools.
        assert_eq!(
            fields["disallowed_tools"],
            "CronCreate,CronDelete,CronList,RemoteTrigger"
        );
        assert_eq!(fields["disable_auto_memory"], "<unset>");
        assert_eq!(fields["initialize"], "<unset>");
        assert_eq!(fields["settings"], "<unset>");
        assert_eq!(fields["settings_count"], "0");
    }

    fn expected_settings(cwd: &std::path::Path) -> Value {
        let canon = std::fs::canonicalize(cwd).unwrap();
        json!({
            "claudeMdExcludes": [format!("!{}/**", glob_escape(canon.to_str().unwrap()))],
            "pluginConfigs": {"cc-plugin-agents-md@builtin": {"options": {"instructionFiles": "claude-md-and-agents-md"}}}
        })
    }

    #[tokio::test]
    async fn an_isolated_spawn_passes_the_scoped_settings_exactly_once() {
        for isolation in [
            Isolation::default(),
            Isolation::Isolated {
                skills: vec!["run-tests".into()],
                memory: true,
            },
        ] {
            let fields = echo_args_for(isolation).await;
            assert_eq!(fields["settings_count"], "1");
            let settings: Value = serde_json::from_str(&fields["settings"]).unwrap();
            assert_eq!(settings, expected_settings(&std::env::temp_dir()));
        }
    }

    #[tokio::test]
    async fn a_chat_spawn_gets_no_settings_flag() {
        let fields = echo_args_for(Isolation::InheritOperatorConfig).await;
        assert_eq!(fields["settings"], "<unset>");
        assert_eq!(fields["settings_count"], "0");
    }

    #[test]
    fn glob_escape_leaves_ordinary_paths_alone() {
        assert_eq!(glob_escape("/Users/me/dev/repo"), "/Users/me/dev/repo");
        assert_eq!(glob_escape("/Users/me/my repo"), "/Users/me/my repo");
        assert_eq!(glob_escape("/a.b-c#d/é"), "/a.b-c#d/é");
    }

    #[test]
    fn glob_escape_neutralises_each_syntax_character_once() {
        for c in ['*', '?', '[', ']', '{', '}', '(', ')', '+', '@', '|', '"'] {
            assert_eq!(glob_escape(&format!("/a{c}b")), format!("/a[{c}]b"));
        }
        for c in ['!', '\\'] {
            assert_eq!(glob_escape(&format!("/a{c}b")), "/a?b");
        }
        assert_eq!(
            glob_escape("/x/[a]+(b)!{c}@d*e?f\\g"),
            "/x/[[]a[]][+][(]b[)]?[{]c[}][@]d[*]e[?]f?g"
        );
    }

    #[test]
    fn isolated_settings_falls_back_with_a_warning_when_the_path_cannot_be_resolved() {
        let missing = std::env::temp_dir().join(format!("choco-missing-{}", uuid::Uuid::new_v4()));
        let (json_str, warnings) = isolated_settings(&missing);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("could not resolve the real path"));
        let settings: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(
            settings["claudeMdExcludes"][0],
            format!("!{}/**", glob_escape(missing.to_str().unwrap()))
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_path_that_is_not_utf8_is_used_lossily_with_a_warning() {
        use std::os::unix::ffi::OsStrExt;
        let bad = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bad-\xff"));
        let mut warnings = Vec::new();
        let path = utf8_path(bad, bad, &mut warnings);
        assert_eq!(path, "/tmp/bad-\u{fffd}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not valid UTF-8"));
        let mut none = Vec::new();
        utf8_path(std::path::Path::new("/tmp/ok"), bad, &mut none);
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn an_isolated_spawn_whose_init_has_no_plugins_key_produces_no_warning() {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            disallowed_tools: Vec::new(),
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: true,
            report_outcomes: vec!["done".to_string()],
            report_sections: Vec::new(),
            isolation: Isolation::default(),
        };
        let mut handle = adapter.start("go", &cfg).unwrap();
        loop {
            match handle.recv().await.expect("stream ended before a reply") {
                AgentEvent::AssistantMessage { .. } => break,
                AgentEvent::Error { message } => panic!("unexpected warning: {message}"),
                _ => {}
            }
        }
    }

    /// The reader, not just `agents_md_plugin_warning`, must send the warning:
    /// a fake CLI whose `init` lists plugins without agents-md, spawned
    /// isolated, yields `SessionMeta` then the `Error`; a chat spawn doesn't.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_reader_sends_the_plugin_warning_after_the_session_meta_for_isolated_roles() {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!("choco-fake-{}.sh", uuid::Uuid::new_v4()));
        std::fs::write(
            &script,
            "#!/bin/sh\nread line\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"plugins\":[{\"name\":\"cc-plugin-telemetry\",\"source\":\"cc-plugin-telemetry@builtin\"}]}'\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"x\",\"session_id\":\"s\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (isolation, expect_warning) in [
            (Isolation::default(), true),
            (Isolation::InheritOperatorConfig, false),
        ] {
            let adapter = ClaudeAdapter::with_binary(script.to_str().unwrap().to_string());
            let cfg = RoleConfig {
                disallowed_tools: Vec::new(),
                cwd: std::env::temp_dir(),
                model: None,
                system_prompt: None,
                sandboxed: true,
                report_outcomes: Vec::new(),
                report_sections: Vec::new(),
                isolation,
            };
            // A sibling test forking while the script was open for writing
            // makes exec fail with ETXTBSY; that is transient, so retry.
            let mut attempts = 0;
            let mut handle = loop {
                match adapter.start("go", &cfg) {
                    Err(AdapterError::Spawn(e))
                        if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 20 =>
                    {
                        attempts += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    other => break other.unwrap(),
                }
            };
            let mut seen = Vec::new();
            while let Some(event) = handle.recv().await {
                let done = matches!(event, AgentEvent::TurnCompleted { .. });
                seen.push(event);
                if done {
                    break;
                }
            }
            let warned = matches!(seen.get(1), Some(AgentEvent::Error { message }) if message.contains("AGENTS.md"));
            assert!(matches!(seen[0], AgentEvent::SessionMeta { .. }));
            assert_eq!(warned, expect_warning, "{seen:?}");
        }
        std::fs::remove_file(&script).unwrap();
    }

    /// Canonicalisation proven on every platform: the cwd is a symlink the
    /// test makes, with a trailing slash, so the flag must name the real dir.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_isolated_spawn_scopes_to_the_real_path_behind_a_symlink() {
        let id = uuid::Uuid::new_v4();
        let real = std::env::temp_dir().join(format!("choco-real-{id}"));
        let link = std::env::temp_dir().join(format!("choco-link-{id}"));
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let fields = echo_args_in(Isolation::default(), Vec::new(), link.clone()).await;
        std::fs::remove_file(&link).unwrap();
        let settings: Value = serde_json::from_str(&fields["settings"]).unwrap();
        let expected = expected_settings(&real);
        std::fs::remove_dir(&real).unwrap();
        assert_eq!(settings, expected);
        let glob = settings["claudeMdExcludes"][0].as_str().unwrap();
        assert!(!glob.contains(&format!("choco-link-{id}")), "{glob}");
    }

    #[test]
    fn the_scope_glob_has_no_doubled_slash_when_resolution_fails() {
        let missing = std::env::temp_dir().join(format!("choco-missing-{}/", uuid::Uuid::new_v4()));
        let (json_str, _) = isolated_settings(&missing);
        let settings: Value = serde_json::from_str(&json_str).unwrap();
        let glob = settings["claudeMdExcludes"][0].as_str().unwrap();
        assert!(glob.ends_with("/**") && !glob.ends_with("//**"), "{glob}");
    }

    /// The spawn-time warning must reach the event channel, first.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_non_utf8_cwd_sends_its_warning_as_the_first_event() {
        use std::os::unix::ffi::OsStrExt;
        let mut name = format!("choco-bad-{}-", uuid::Uuid::new_v4()).into_bytes();
        name.push(0xff);
        let dir = std::env::temp_dir().join(std::ffi::OsStr::from_bytes(&name));
        std::fs::create_dir(&dir).unwrap();
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            disallowed_tools: Vec::new(),
            cwd: dir.clone(),
            model: None,
            system_prompt: None,
            sandboxed: true,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::default(),
        };
        let mut handle = adapter.start("go", &cfg).unwrap();
        let first = handle.recv().await;
        std::fs::remove_dir(&dir).unwrap();
        assert!(
            matches!(&first, Some(AgentEvent::Error { message }) if message.contains("not valid UTF-8")),
            "{first:?}"
        );
    }

    /// A sub-agent's `init` line (it has a `parent_tool_use_id`) carries its
    /// own plugin set and must not put a warning on the main role's timeline.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sub_agent_init_line_does_not_trigger_the_plugin_warning() {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!("choco-fake-{}.sh", uuid::Uuid::new_v4()));
        std::fs::write(
            &script,
            "#!/bin/sh\nread line\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"parent_tool_use_id\":\"toolu_x\",\"plugins\":[{\"name\":\"cc-plugin-telemetry\",\"source\":\"cc-plugin-telemetry@builtin\"}]}'\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"x\",\"session_id\":\"s\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let adapter = ClaudeAdapter::with_binary(script.to_str().unwrap().to_string());
        let cfg = RoleConfig {
            disallowed_tools: Vec::new(),
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: true,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::default(),
        };
        let mut handle = adapter.start("go", &cfg).unwrap();
        let mut seen = Vec::new();
        while let Some(event) = handle.recv().await {
            let done = matches!(event, AgentEvent::TurnCompleted { .. });
            seen.push(event);
            if done {
                break;
            }
        }
        std::fs::remove_file(&script).unwrap();
        assert!(
            !seen.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
            "{seen:?}"
        );
        assert!(matches!(
            seen.last(),
            Some(AgentEvent::TurnCompleted { .. })
        ));
    }

    fn real_plugins() -> Vec<Value> {
        ["agents-md", "telemetry", "plugin-authoring"]
            .iter()
            .map(|n| {
                json!({"name": format!("cc-plugin-{n}"), "path": "builtin", "source": format!("cc-plugin-{n}@builtin")})
            })
            .collect()
    }

    #[test]
    fn agents_md_plugin_warning_covers_each_shape() {
        let iso = Isolation::default();
        let init =
            |plugins: Value| json!({"type": "system", "subtype": "init", "plugins": plugins});

        let without = init(json!(real_plugins()[1..]));
        let Some(AgentEvent::Error { message }) = agents_md_plugin_warning(&without, &iso) else {
            panic!("expected a warning");
        };
        assert!(message.contains("cc-plugin-agents-md") && message.contains("AGENTS.md"));
        assert!(agents_md_plugin_warning(&init(json!([])), &iso).is_some());
        assert!(agents_md_plugin_warning(&init(json!(real_plugins())), &iso).is_none());
        assert!(
            agents_md_plugin_warning(
                &init(json!([{"name": "agents-md", "source": "agents-md@builtin"}])),
                &iso
            )
            .is_none()
        );
        assert!(agents_md_plugin_warning(&init(json!(["agents-md@builtin"])), &iso).is_none());
        assert!(
            agents_md_plugin_warning(&json!({"type": "system", "subtype": "init"}), &iso).is_none()
        );
        assert!(agents_md_plugin_warning(&without, &Isolation::InheritOperatorConfig).is_none());
    }

    /// #115: workflow role shapes lose every timer tool, and chat loses
    /// every tool in `CHAT_BLOCKED_TOOLS`, so a tool added to a constant
    /// can't be dropped from one shape.
    #[tokio::test]
    async fn every_role_shape_loses_the_tools_it_should() {
        let shapes = [
            (Isolation::default(), TIMER_TOOLS.to_vec()),
            (
                Isolation::Isolated {
                    skills: vec!["run-tests".to_string()],
                    memory: true,
                },
                TIMER_TOOLS.to_vec(),
            ),
            (
                Isolation::InheritOperatorConfig,
                CHAT_BLOCKED_TOOLS.to_vec(),
            ),
        ];
        for (isolation, expected) in shapes {
            let fields = echo_args_for(isolation).await;
            let listed: Vec<&str> = fields["disallowed_tools"].split(',').collect();
            for tool in expected {
                assert!(listed.contains(&tool), "{tool} missing from {listed:?}");
            }
        }
        for tool in CHAT_BLOCKED_TOOLS {
            assert!(TIMER_TOOLS.contains(&tool), "{tool} not a timer tool");
        }
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
        let events = normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown);
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
            normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown),
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
        let events = normalize(&parse(line), &mut tool_names, &mut BillingMode::Unknown);
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
            normalize(&parse(rejected), &mut tool_names, &mut BillingMode::Unknown),
            vec![AgentEvent::Error {
                message: "the CLI rejected the session's initialize request: unknown field"
                    .to_string()
            }]
        );
        assert_eq!(
            normalize(&parse(accepted), &mut tool_names, &mut BillingMode::Unknown),
            Vec::new()
        );
    }

    /// Removes a scratch tree and the transcript folder a probe created, even
    /// on panic.
    struct Scratch {
        root: PathBuf,
        uuid: String,
        transcript_dir: Option<PathBuf>,
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
            if let Some(dir) = &self.transcript_dir
                && dir
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains(&self.uuid))
            {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn write_file(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// One probe against the real CLI: builds a scratch tree named `name`
    /// under `$HOME`, runs an isolated role in its `repo/`, and checks the
    /// session transcript for which instruction files were loaded.
    async fn probe_instruction_files(name: String, uuid: &str, config: &std::path::Path) {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let root = home.join(&name);
        let mut guard = Scratch {
            root: root.clone(),
            uuid: uuid.to_string(),
            transcript_dir: None,
        };
        let repo = root.join("repo");
        let marker = |tag: &str| format!("MARKER-{tag}-{uuid}");
        write_file(&root.join("CLAUDE.md"), &marker("parent-claude"));
        write_file(&root.join("AGENTS.md"), &marker("parent-agents"));
        write_file(&repo.join("CLAUDE.md"), &marker("repo-claude"));
        write_file(&repo.join("AGENTS.md"), &marker("repo-agents"));
        write_file(&repo.join("sub/CLAUDE.md"), &marker("sub-claude"));
        write_file(&repo.join("sub/AGENTS.md"), &marker("sub-agents"));
        write_file(&repo.join("sub/notes.txt"), "the notes file");
        write_file(&repo.join(".claude/CLAUDE.md"), &marker("dot-claude"));
        let git = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(git.success());
        let repo = std::fs::canonicalize(&repo).unwrap();

        let adapter = ClaudeAdapter::new();
        let cfg = RoleConfig {
            cwd: repo.clone(),
            model: Some("haiku".to_string()),
            isolation: Isolation::default(),
            sandboxed: true,
            report_outcomes: vec![],
            report_sections: vec![],
            disallowed_tools: vec![],
            system_prompt: None,
        };
        let mut handle = adapter
            .start(
                "Use the Read tool to read sub/notes.txt, then reply with its contents.",
                &cfg,
            )
            .unwrap();
        let mut session_id = None;
        let mut plugins = Value::Null;
        let mut warnings = Vec::new();
        while let Some(event) = handle.recv().await {
            match event {
                AgentEvent::SessionMeta {
                    adapter_session_id,
                    details,
                } => {
                    plugins = details["init"]["plugins"].clone();
                    session_id = Some(adapter_session_id);
                }
                AgentEvent::Error { message } if message.contains("agents-md plugin") => {
                    warnings.push(message)
                }
                AgentEvent::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        println!("[{name}] init plugins: {plugins}");
        let session_id = session_id.expect("no session id from the init line");

        let transcript = std::fs::read_dir(config.join("projects"))
            .unwrap()
            .flatten()
            .map(|d| d.path())
            .find(|d| d.join(format!("{session_id}.jsonl")).exists())
            .expect("transcript not found");
        guard.transcript_dir = Some(transcript.clone());
        let text = std::fs::read_to_string(transcript.join(format!("{session_id}.jsonl"))).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let loaded_at = |path: &std::path::Path| {
            let needle = format!("Contents of {}", path.display());
            lines.iter().position(|l| l.contains(&needle))
        };
        let read_at = lines
            .iter()
            .position(|l| l.contains("\"tool_use\"") && l.contains("notes.txt"))
            .expect(
                "the transcript shows no Read of sub/notes.txt, so the nested checks mean nothing",
            );

        assert!(
            loaded_at(&repo.join("CLAUDE.md")).is_some(),
            "[{name}] repo CLAUDE.md not loaded"
        );
        assert!(
            loaded_at(&repo.join("AGENTS.md")).is_some(),
            "[{name}] repo AGENTS.md not loaded"
        );
        for file in ["sub/CLAUDE.md", "sub/AGENTS.md"] {
            let at =
                loaded_at(&repo.join(file)).unwrap_or_else(|| panic!("[{name}] {file} not loaded"));
            assert!(at >= read_at, "[{name}] {file} loaded before the read");
        }
        let canon_root = std::fs::canonicalize(&root).unwrap();
        for file in ["CLAUDE.md", "AGENTS.md"] {
            assert!(
                loaded_at(&canon_root.join(file)).is_none(),
                "[{name}] parent {file} leaked"
            );
        }
        let personal = config.join("CLAUDE.md");
        if personal.exists() {
            assert!(
                loaded_at(&personal).is_none(),
                "[{name}] personal CLAUDE.md leaked"
            );
        } else {
            println!(
                "[{name}] {} does not exist: the personal-file assertion is vacuous",
                personal.display()
            );
        }
        println!(
            "[{name}] repo/.claude/CLAUDE.md loaded: {}",
            loaded_at(&repo.join(".claude/CLAUDE.md")).is_some()
        );
        assert!(warnings.is_empty(), "[{name}] plugin warning: {warnings:?}");
    }

    /// Opt-in contract test against the real `claude` CLI (needs a login):
    /// an isolated role loads exactly the repo's own `CLAUDE.md`/`AGENTS.md`.
    ///
    /// `cargo build --workspace --all-targets && cargo test -p chocofactoryd --lib -- --ignored isolated_role_loads_exactly_the_repos_instruction_files --nocapture`
    #[tokio::test]
    #[ignore = "needs a logged-in real claude CLI"]
    async fn isolated_role_loads_exactly_the_repos_instruction_files() {
        let config = std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".claude"));
        let uuid = uuid::Uuid::new_v4().to_string();
        probe_instruction_files(format!("choco-agents-md-{uuid}"), &uuid, &config).await;
        probe_instruction_files(format!("choco-agents-md-{uuid} [x]+(1)|"), &uuid, &config).await;
        probe_instruction_files(
            format!("choco-agents-md-{uuid} {{a,b}}!@*]"),
            &uuid,
            &config,
        )
        .await;
    }
}
