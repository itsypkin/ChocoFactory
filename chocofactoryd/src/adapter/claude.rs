use std::collections::HashMap;
use std::process::Stdio;

use chocofactory_core::mcp::{
    CLAUDE_SETTINGS_REJECTED_TOOL_NAME, MCP_SERVER_NAME, REPORT_OUTCOME_TOOL_NAME,
    qualified_claude_settings_rejected_tool_name,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::claude_settings::ClaudeSettings;

use super::{
    AdapterError, AgentAdapter, AgentEvent, AgentHandle, BackgroundJob, BillingMode,
    InterruptionEvidence, Isolation, ModelUsage, RoleConfig, RoleTool, SettingsVerdict,
    TokenCounts, TurnUsage, UsageCounting, report_instruction, usage_limit_text,
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

    fn validate_role(
        &self,
        role: &str,
        isolation: &Isolation,
        claude_settings: &ClaudeSettings,
    ) -> Result<(), String> {
        // An isolated role's `env` gets choco's sentinel variable added (see
        // `isolated_settings`), which needs a mapping to add it to. A `null`
        // is dropped when the layers merge, so it is not refused here.
        if matches!(isolation, Isolation::Isolated { .. })
            && let Some(env) = claude_settings.map().get("env")
            && !env.is_object()
            && !env.is_null()
        {
            return Err(format!(
                "role '{role}': claude_settings 'env' must be a mapping, because choco adds a variable to it to check that Claude Code applied the settings"
            ));
        }
        Ok(())
    }

    fn validate_model(&self, role: &str, model: &str) -> Result<(), String> {
        super::level::claude_model_and_level(role, model).map(|_| ())
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
    // Parse the level before anything is created, so a bad one spawns nothing.
    let model_and_level = match &cfg.model {
        Some(model) => Some(
            super::level::claude_model_and_level(&cfg.role, model)
                .map_err(|message| AdapterError::Spawn(std::io::Error::other(message)))?,
        ),
        None => None,
    };
    let marker = super::new_turn_marker()?;
    // D11 (#346): an isolated role with settings of its own gets a per-spawn
    // sentinel variable in the settings' `env`, checked by choco's tool server.
    // Chat has no choco scope to lose, so it gets none.
    let sentinel = match &cfg.isolation {
        Isolation::Isolated { .. } if !cfg.claude_settings.is_empty() => {
            Some(new_settings_sentinel()?)
        }
        _ => None,
    };
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
    // The expected sentinel reaches the tool server as an argument, never
    // through the MCP config's `env` or the `claude` process's environment:
    // either of those would set the variable whether or not Claude Code
    // applied the settings, and the check could not tell.
    if let Some((name, value)) = &sentinel {
        mcp_args.push("--claude-settings-sentinel".to_string());
        mcp_args.push(format!("{name}={value}"));
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
    // - `--settings` (`isolated_settings`) starts from the role's own
    //   `claude_settings` (#346: the operator's way to pass non-secret login,
    //   routing and attribution settings to choco's agents, since the user
    //   settings file is not read), then choco writes its scope over it. It
    //   scopes the instruction files: its
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
    apply_spawn_env(&mut command, &cfg.isolation);
    command.env(&marker, "1");
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
            // The role's own settings, alone and exactly once: no choco scope
            // keys, and they beat the operator's user settings (#346).
            if !cfg.claude_settings.is_empty() {
                command
                    .arg("--settings")
                    .arg(Value::Object(cfg.claude_settings.map().clone()).to_string());
            }
            None
        }
        Isolation::Isolated { skills, memory: _ } => {
            let (settings, warnings) = isolated_settings(
                &cfg.role,
                &cfg.cwd,
                &cfg.claude_settings,
                sentinel
                    .as_ref()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .map_err(|message| AdapterError::Spawn(std::io::Error::other(message)))?;
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

    match model_and_level {
        Some((id, level)) => {
            command.arg("--model").arg(id).arg("--effort").arg(level);
        }
        None => {
            command.arg("--effort").arg(super::level::DEFAULT_LEVEL);
        }
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

    // With a sentinel, no user message is written until Claude Code's tool
    // list shows the settings were applied (see `GateSenders`).
    let (gate_tx, gate_rx) = match sentinel {
        Some(_) => {
            let (init_tx, init_rx) = oneshot::channel();
            let (verdict_tx, verdict_rx) = oneshot::channel();
            (
                Some(GateSenders {
                    init_answered: Some(init_tx),
                    verdict: Some(verdict_tx),
                    role: cfg.role.clone(),
                }),
                Some(GateReceivers {
                    init_answered: init_rx,
                    verdict: verdict_rx,
                }),
            )
        }
        None => (None, None),
    };

    tokio::spawn(run_stdin_writer(stdin, initialize, gate_rx, stdin_rx));
    tokio::spawn(run_stderr_reader(stderr, events_tx.clone()));
    tokio::spawn(run_stdout_reader(
        stdout,
        events_tx,
        cfg.isolation.describe(),
        cfg.isolation.clone(),
        cfg.claude_settings.keys_sorted(),
        gate_tx,
    ));

    Ok(AgentHandle::new(child, events_rx, stdin_tx, marker))
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

/// The environment decisions `spawn` makes for every role shape, in one place
/// so a test can run them per isolation.
fn apply_spawn_env(command: &mut Command, isolation: &Isolation) {
    apply_auto_memory_env(command, isolation);
    apply_effort_env(command);
}

/// Removes `CLAUDE_CODE_EFFORT_LEVEL` from the spawned process's
/// environment for every role (#336). The level reaches claude only as
/// `--effort`; the daemon's launch environment is not the operator's choice
/// for this role (same reasoning as `apply_auto_memory_env`, #105).
///
/// Probed on Claude Code 2.1.308 against a localhost stub, reading the
/// request's `output_config.effort`:
///
/// - process env `CLAUDE_CODE_EFFORT_LEVEL=max` + `--effort low` sent `max`:
///   the env beats the flag, hence the removal.
/// - an invalid or empty env value is ignored.
/// - `effortLevel` in settings has no effect, and `--effort` beats it.
/// - `{"env":{"CLAUDE_CODE_EFFORT_LEVEL":"max"}}` in a project
///   `.claude/settings.json` + `--effort low` sent `max`. This can't be
///   scrubbed from here: an isolated role applies the repo's committed
///   settings, and the chat role the operator's own, so such an `env` value
///   still wins (documented in `docs/models.md`). The same holds for an `env`
///   block in the role's own `claude_settings` (#346), which choco passes in
///   `--settings`.
fn apply_effort_env(command: &mut Command) {
    command.env_remove("CLAUDE_CODE_EFFORT_LEVEL");
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
        "request_id": INITIALIZE_REQUEST_ID,
        "request": { "subtype": "initialize", "skills": skills },
    });
    format!("{request}\n")
}

/// The `request_id` of choco's `mcp_status` control request.
const MCP_STATUS_REQUEST_ID: &str = "chocofactory-mcp-status";
/// The `request_id` of choco's `initialize` control request.
const INITIALIZE_REQUEST_ID: &str = "chocofactory-initialize";

/// The stdout reader's side of the settings gate (D11, #346).
///
/// Claude Code starts and connects its MCP servers before it has a prompt,
/// and answers the `initialize` control request only after they have
/// connected or failed (probed on 2.1.310). The writer therefore sends
/// `mcp_status` once the `initialize` reply is in, and writes no user
/// message until the `mcp_status` reply passes. Claude Code has no user
/// message before then, so it makes no model request and runs no tool:
/// the agent cannot act on a turn whose settings the `mcp_status` check finds
/// not applied. (The `init`-line backstop is the second detector, and it can
/// fire after the prompt was written.)
struct GateSenders {
    init_answered: Option<oneshot::Sender<()>>,
    /// Sent when the check passed. Dropped unsent on a miss, which makes the
    /// writer return without writing and so closes stdin.
    verdict: Option<oneshot::Sender<()>>,
    role: String,
}

struct GateReceivers {
    init_answered: oneshot::Receiver<()>,
    verdict: oneshot::Receiver<()>,
}

/// The `mcp_status` control request line.
fn mcp_status_line() -> String {
    let request = json!({
        "type": "control_request",
        "request_id": MCP_STATUS_REQUEST_ID,
        "request": { "subtype": "mcp_status" },
    });
    format!("{request}\n")
}

async fn run_stdin_writer(
    mut stdin: tokio::process::ChildStdin,
    initialize: Option<String>,
    gate: Option<GateReceivers>,
    mut stdin_rx: mpsc::UnboundedReceiver<String>,
) {
    // Ahead of the first user turn, so the allowlist is in force before the
    // model ever sees a skill listing.
    if let Some(line) = initialize
        && stdin.write_all(line.as_bytes()).await.is_err()
    {
        return;
    }
    if let Some(gate) = gate {
        // Messages queued meanwhile (the prompt, a nudge, a send) are held,
        // not written. If every sender goes away while waiting (the handle
        // closed stdin), nothing is written and stdin closes.
        let mut held: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let GateReceivers {
            init_answered,
            verdict,
        } = gate;
        if !wait_holding(init_answered, &mut stdin_rx, &mut held).await {
            return;
        }
        if stdin.write_all(mcp_status_line().as_bytes()).await.is_err() {
            return;
        }
        if !wait_holding(verdict, &mut stdin_rx, &mut held).await {
            return;
        }
        for text in held {
            if stdin
                .write_all(user_turn_line(&text).as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    }
    while let Some(text) = stdin_rx.recv().await {
        let line = user_turn_line(&text);
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
}

/// Waits for `signal`, holding whatever arrives on `stdin_rx` meanwhile.
/// `false` when the signal's sender was dropped (a miss, or the reader
/// ended) or every sender of `stdin_rx` went away first.
async fn wait_holding(
    mut signal: oneshot::Receiver<()>,
    stdin_rx: &mut mpsc::UnboundedReceiver<String>,
    held: &mut std::collections::VecDeque<String>,
) -> bool {
    loop {
        tokio::select! {
            result = &mut signal => return result.is_ok(),
            message = stdin_rx.recv() => match message {
                Some(text) => held.push_back(text),
                None => return false,
            },
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
    claude_settings_keys: Vec<String>,
    mut gate: Option<GateSenders>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut billing = BillingMode::Unknown;
    // At most one `SettingsNotApplied` per spawn.
    let mut settings_event_sent = false;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(gate) = gate.as_mut()
            && value.get("type").and_then(Value::as_str) == Some("control_response")
        {
            match value
                .pointer("/response/request_id")
                .and_then(Value::as_str)
            {
                Some(INITIALIZE_REQUEST_ID) => {
                    if let Some(tx) = gate.init_answered.take() {
                        let _ = tx.send(());
                    }
                }
                Some(MCP_STATUS_REQUEST_ID) => {
                    // The reply holds the tool server's argv (`config.args`),
                    // which holds the sentinel: it is read here and copied
                    // nowhere.
                    match settings_verdict(&value) {
                        Ok(()) => {
                            if let Some(tx) = gate.verdict.take() {
                                let _ = tx.send(());
                            }
                        }
                        Err(verdict) => {
                            gate.verdict = None;
                            if !settings_event_sent {
                                settings_event_sent = true;
                                let event = AgentEvent::SettingsNotApplied {
                                    verdict,
                                    role: gate.role.clone(),
                                };
                                if events_tx.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        for mut event in normalize(&value, &mut tool_names, &mut billing) {
            if let AgentEvent::SessionMeta {
                details: Value::Object(details),
                ..
            } = &mut event
            {
                details.insert("isolation".to_string(), isolation.clone());
                details.insert(
                    "claude_settings_keys".to_string(),
                    json!(claude_settings_keys),
                );
            }
            if events_tx.send(event).is_err() {
                return;
            }
        }
        let top_level_init = value.get("type").and_then(Value::as_str) == Some("system")
            && value.get("subtype").and_then(Value::as_str) == Some("init")
            && value.get("parent_tool_use_id").is_none_or(Value::is_null);
        // Backstop for the gate: an `init` line that lists the rejected-
        // settings tool means the tool server found no sentinel, whatever
        // the status reply said.
        if top_level_init
            && let Some(gate) = gate.as_mut()
            && !settings_event_sent
            && init_lists_rejected_tool(&value)
        {
            settings_event_sent = true;
            gate.verdict = None;
            let event = AgentEvent::SettingsNotApplied {
                verdict: SettingsVerdict::Rejected,
                role: gate.role.clone(),
            };
            if events_tx.send(event).is_err() {
                return;
            }
        }
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

/// A per-spawn sentinel for the settings check: a variable name
/// `CHOCOFACTORY_SETTINGS_<32 hex>` and a value of 32 hex digits, from two
/// independent random reads. A failed read fails the spawn.
fn new_settings_sentinel() -> Result<(String, String), AdapterError> {
    let name = format!("CHOCOFACTORY_SETTINGS_{}", super::random_hex_128()?);
    let value = super::random_hex_128()?;
    Ok((name, value))
}

/// Reads Claude Code's reply to choco's `mcp_status` request. `Ok` means the
/// tool server is connected and offers `report_outcome` (so it found the
/// sentinel). Never copies anything out of the reply but a status word of
/// plain lowercase letters: the reply's `config.args` holds the sentinel.
fn settings_verdict(reply: &Value) -> Result<(), SettingsVerdict> {
    let unconfirmed = |status: &str| SettingsVerdict::Unconfirmed {
        status: status.to_string(),
    };
    if reply.pointer("/response/subtype").and_then(Value::as_str) != Some("success") {
        return Err(unconfirmed("the CLI refused the mcp_status request"));
    }
    let Some(server) = reply
        .pointer("/response/response/mcpServers")
        .and_then(Value::as_array)
        .and_then(|servers| {
            servers
                .iter()
                .find(|server| server.get("name").and_then(Value::as_str) == Some(MCP_SERVER_NAME))
        })
    else {
        return Err(unconfirmed("not listed"));
    };
    let tools: Vec<&str> = server
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if tools.contains(&CLAUDE_SETTINGS_REJECTED_TOOL_NAME) {
        return Err(SettingsVerdict::Rejected);
    }
    match server.get("status").and_then(Value::as_str) {
        Some("connected") => {}
        Some(status)
            if !status.is_empty()
                && status.len() <= 32
                && status
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'_') =>
        {
            return Err(unconfirmed(status));
        }
        _ => return Err(unconfirmed("unrecognised status")),
    }
    if tools.contains(&REPORT_OUTCOME_TOOL_NAME) {
        Ok(())
    } else {
        Err(unconfirmed("connected without report_outcome"))
    }
}

/// Whether an `init` line's tool list holds the rejected-settings tool.
fn init_lists_rejected_tool(init: &Value) -> bool {
    let rejected = qualified_claude_settings_rejected_tool_name();
    init.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.as_str() == Some(rejected.as_str()))
        })
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

/// The `--settings` JSON for an isolated role (#183, #346) and any warnings
/// about falling back while building it.
///
/// It starts from the role's `claude_settings`, then choco writes last what
/// it must own: the sentinel inside `env` (D11), `claudeMdExcludes` (replaced
/// whole: only instruction files under the role's canonical working directory
/// load), and the built-in agents-md plugin's entry in `pluginConfigs` (other
/// plugin entries the workflow wrote are kept; a `pluginConfigs` that is not
/// a mapping is replaced). With no settings and no sentinel the bytes are
/// what they were before `claude_settings` existed.
///
/// A role `env` that is not a mapping is an error, never replaced:
/// `validate_role` refuses it earlier, so this is a backstop.
fn isolated_settings(
    role: &str,
    cwd: &std::path::Path,
    claude_settings: &ClaudeSettings,
    sentinel: Option<(&str, &str)>,
) -> Result<(String, Vec<String>), String> {
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
    let mut settings = claude_settings.map().clone();
    if let Some((name, value)) = sentinel {
        match settings.get_mut("env") {
            None => {
                settings.insert("env".to_string(), json!({ name: value }));
            }
            Some(Value::Object(env)) => {
                env.insert(name.to_string(), Value::String(value.to_string()));
            }
            Some(_) => {
                return Err(format!(
                    "role '{role}': claude_settings 'env' must be a mapping, because choco adds a variable to it to check that Claude Code applied the settings"
                ));
            }
        }
    }
    settings.insert(
        "claudeMdExcludes".to_string(),
        json!([format!("!{}/**", escaped.trim_end_matches('/'))]),
    );
    let agents_md = json!({ "options": { "instructionFiles": "claude-md-and-agents-md" } });
    match settings.get_mut("pluginConfigs") {
        Some(Value::Object(plugins)) => {
            plugins.insert("cc-plugin-agents-md@builtin".to_string(), agents_md);
        }
        _ => {
            settings.insert(
                "pluginConfigs".to_string(),
                json!({ "cc-plugin-agents-md@builtin": agents_md }),
            );
        }
    }
    Ok((Value::Object(settings).to_string(), warnings))
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
        Some("system")
            if value.get("subtype").and_then(Value::as_str) == Some("background_tasks_changed") =>
        {
            // The level is per CLI process: a list scoped to a sub-agent
            // must not replace the main one.
            if value
                .get("parent_tool_use_id")
                .is_some_and(|v| !v.is_null())
            {
                tracing::debug!("ignoring a sub-agent-scoped background_tasks_changed line");
                return Vec::new();
            }
            vec![AgentEvent::BackgroundJobs {
                running: parse_background_jobs(value),
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

/// The jobs listed by a `background_tasks_changed` line. Unparseable data
/// never counts as a running job: it must not keep a turn alive.
fn parse_background_jobs(value: &Value) -> Vec<BackgroundJob> {
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        tracing::warn!("background_tasks_changed line has no `tasks` array; treating it as empty");
        return Vec::new();
    };
    let mut running = Vec::new();
    for task in tasks {
        let Some(id) = task.get("task_id").and_then(Value::as_str) else {
            tracing::warn!(%task, "dropping a background task entry without a string task_id");
            continue;
        };
        let flag = |key: &str| task.get(key).and_then(Value::as_bool) == Some(true);
        if flag("ambient") || flag("awaited") {
            tracing::debug!(
                task_id = id,
                "dropping an ambient or awaited background task"
            );
            continue;
        }
        let kind = task
            .get("task_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let description = task
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .unwrap_or(id);
        running.push(BackgroundJob {
            id: id.to_string(),
            kind: kind.to_string(),
            description: description.to_string(),
        });
    }
    running
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
        echo_args_model(isolation, disallowed_tools, cwd, None, false).await
    }

    /// Runs the echo fixture through `start` (or `resume`) with this model.
    async fn echo_args_model(
        isolation: Isolation,
        disallowed_tools: Vec<RoleTool>,
        cwd: std::path::PathBuf,
        model: Option<&str>,
        resume: bool,
    ) -> HashMap<String, String> {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"));
        let cfg = RoleConfig {
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
            disallowed_tools,
            cwd,
            model: model.map(str::to_string),
            system_prompt: None,
            sandboxed: true,
            report_outcomes: vec!["done".to_string()],
            report_sections: Vec::new(),
            isolation,
        };
        let mut handle = if resume {
            adapter.resume("sess-1", "go", &cfg).unwrap()
        } else {
            adapter.start("go", &cfg).unwrap()
        };
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

    fn effort_env_entry(isolation: &Isolation) -> Option<Option<String>> {
        let mut command = Command::new("unused");
        apply_spawn_env(&mut command, isolation);
        command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    /// #336: the daemon's own `CLAUDE_CODE_EFFORT_LEVEL` beats `--effort`, so
    /// it is explicitly removed (not left to inherit) for every role shape.
    #[test]
    fn every_role_shape_removes_the_effort_env() {
        for isolation in [
            Isolation::Isolated {
                skills: Vec::new(),
                memory: false,
            },
            Isolation::Isolated {
                skills: Vec::new(),
                memory: true,
            },
            Isolation::InheritOperatorConfig,
        ] {
            assert_eq!(effort_env_entry(&isolation), Some(None), "{isolation:?}");
        }
    }

    /// The args every role gets: id and level split apart, each flag once.
    #[tokio::test]
    async fn a_models_level_suffix_becomes_the_effort_flag() {
        let isolations = || [Isolation::default(), Isolation::InheritOperatorConfig];
        for resume in [false, true] {
            for isolation in isolations() {
                for (model, id, effort) in [
                    ("claude-sonnet-5-5:xhigh", "claude-sonnet-5-5", "xhigh"),
                    ("claude-sonnet-5-5", "claude-sonnet-5-5", "high"),
                    ("sonnet[1m]:low", "sonnet[1m]", "low"),
                    (
                        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                        "high",
                    ),
                    (
                        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123",
                        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123",
                        "high",
                    ),
                ] {
                    let fields = echo_args_model(
                        isolation.clone(),
                        Vec::new(),
                        std::env::temp_dir(),
                        Some(model),
                        resume,
                    )
                    .await;
                    assert_eq!(fields["model"], id, "{model}");
                    assert_eq!(fields["effort"], effort, "{model}");
                    assert_eq!(fields["effort_count"], "1", "{model}");
                }
                for level in super::super::level::CLAUDE_EFFORT_LEVELS {
                    let fields = echo_args_model(
                        isolation.clone(),
                        Vec::new(),
                        std::env::temp_dir(),
                        Some(&format!("opus:{level}")),
                        resume,
                    )
                    .await;
                    assert_eq!(fields["effort"], level);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_role_without_a_model_still_gets_the_default_effort() {
        let fields = echo_args_for(Isolation::default()).await;
        assert_eq!(fields["model"], "<unset>");
        assert_eq!(fields["effort"], "high");
        assert_eq!(fields["effort_count"], "1");
    }

    #[test]
    fn an_omp_only_level_fails_the_spawn_naming_role_and_suffix() {
        let adapter = ClaudeAdapter::with_binary("/nonexistent/never-spawned");
        let cfg = RoleConfig {
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
            disallowed_tools: Vec::new(),
            cwd: std::env::temp_dir(),
            model: Some("claude-sonnet-5-5:minimal".to_string()),
            system_prompt: None,
            sandboxed: true,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::default(),
        };
        // A spawn error about the binary would be `NotFound`; this one is
        // the level, and it comes before any process is attempted.
        for result in [adapter.start("go", &cfg), adapter.resume("s", "go", &cfg)] {
            let Err(AdapterError::Spawn(err)) = result else {
                panic!("expected a spawn error");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::Other);
            let message = err.to_string();
            assert!(
                message.contains("`coder`") && message.contains(":minimal"),
                "{message}"
            );
        }
    }

    /// One real model call (#336): a role on `claude-sonnet-5-5:high` runs a
    /// turn through the real `claude` without error. Uses the operator's
    /// login.
    ///
    /// Run with: `cargo test -p chocofactoryd --lib a_real_claude_turn_runs_with_an_effort_suffix -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a real claude and its login, and makes one model call"]
    async fn a_real_claude_turn_runs_with_an_effort_suffix() {
        let adapter = ClaudeAdapter::new();
        let cfg = RoleConfig {
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
            disallowed_tools: Vec::new(),
            cwd: std::env::temp_dir(),
            model: Some("claude-sonnet-5-5:high".to_string()),
            system_prompt: None,
            sandboxed: true,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::default(),
        };
        let mut handle = adapter
            .start("Reply with the single word: ok", &cfg)
            .unwrap();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(120), handle.recv())
                .await
                .expect("no event in 120 s")
                .expect("the stream ended before the turn completed");
            match event {
                AgentEvent::Error { message, .. } => panic!("the turn failed: {message}"),
                AgentEvent::TurnCompleted { .. } => break,
                _ => {}
            }
        }
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
        let (json_str, warnings) =
            isolated_settings("coder", &missing, &ClaudeSettings::default(), None).unwrap();
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
        let script = std::env::temp_dir().join(format!("choco-fake-{}.sh", uuid::Uuid::new_v4()));
        crate::test_support::write_executable(
            &script,
            "#!/bin/sh\nread line\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"plugins\":[{\"name\":\"cc-plugin-telemetry\",\"source\":\"cc-plugin-telemetry@builtin\"}]}'\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"x\",\"session_id\":\"s\"}'\n",
        );
        for (isolation, expect_warning) in [
            (Isolation::default(), true),
            (Isolation::InheritOperatorConfig, false),
        ] {
            let adapter = ClaudeAdapter::with_binary(script.to_str().unwrap().to_string());
            let cfg = RoleConfig {
                claude_settings: Default::default(),
                role: "coder".to_string(),
                project_id: None,
                disallowed_tools: Vec::new(),
                cwd: std::env::temp_dir(),
                model: None,
                system_prompt: None,
                sandboxed: true,
                report_outcomes: Vec::new(),
                report_sections: Vec::new(),
                isolation,
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
        let (json_str, _) =
            isolated_settings("coder", &missing, &ClaudeSettings::default(), None).unwrap();
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
            role: "coder".to_string(),
            project_id: None,
            disallowed_tools: Vec::new(),
            cwd: dir.clone(),
            model: None,
            system_prompt: None,
            sandboxed: true,
            report_outcomes: Vec::new(),
            report_sections: Vec::new(),
            isolation: Isolation::default(),
            claude_settings: ClaudeSettings::default(),
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
        let script = std::env::temp_dir().join(format!("choco-fake-{}.sh", uuid::Uuid::new_v4()));
        crate::test_support::write_executable(
            &script,
            "#!/bin/sh\nread line\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"parent_tool_use_id\":\"toolu_x\",\"plugins\":[{\"name\":\"cc-plugin-telemetry\",\"source\":\"cc-plugin-telemetry@builtin\"}]}'\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"x\",\"session_id\":\"s\"}'\n",
        );
        let adapter = ClaudeAdapter::with_binary(script.to_str().unwrap().to_string());
        let cfg = RoleConfig {
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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
            claude_settings: Default::default(),
            role: "coder".to_string(),
            project_id: None,
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

    fn jobs_of(line: &str) -> Vec<Vec<BackgroundJob>> {
        normalize(&parse(line), &mut HashMap::new(), &mut BillingMode::Unknown)
            .into_iter()
            .map(|event| match event {
                AgentEvent::BackgroundJobs { running } => running,
                other => panic!("expected BackgroundJobs, got {other:?}"),
            })
            .collect()
    }

    fn job(id: &str, kind: &str, description: &str) -> BackgroundJob {
        BackgroundJob {
            id: id.into(),
            kind: kind.into(),
            description: description.into(),
        }
    }

    #[test]
    fn a_background_tasks_line_lists_every_job_in_order() {
        let line = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bvsrzxjlr","task_type":"local_bash","description":"Sleep 15 seconds then exit with code 3"},{"task_id":"a7e3de95f2a6e9192","task_type":"local_agent","subagent_type":"general-purpose","description":"Sleep 10 and reply DONE"}]}"#;
        assert_eq!(
            jobs_of(line),
            vec![vec![
                job(
                    "bvsrzxjlr",
                    "local_bash",
                    "Sleep 15 seconds then exit with code 3"
                ),
                job(
                    "a7e3de95f2a6e9192",
                    "local_agent",
                    "Sleep 10 and reply DONE"
                ),
            ]]
        );
    }

    #[test]
    fn an_empty_or_malformed_task_list_is_an_empty_event() {
        let empty = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        assert_eq!(jobs_of(empty), vec![Vec::<BackgroundJob>::new()]);
        let missing = r#"{"type":"system","subtype":"background_tasks_changed"}"#;
        assert_eq!(jobs_of(missing), vec![Vec::<BackgroundJob>::new()]);
        let string = r#"{"type":"system","subtype":"background_tasks_changed","tasks":"x"}"#;
        assert_eq!(jobs_of(string), vec![Vec::<BackgroundJob>::new()]);
        let mixed = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_type":"local_bash"},"x",{"task_id":"ok","task_type":"local_bash","description":"d"}]}"#;
        assert_eq!(jobs_of(mixed), vec![vec![job("ok", "local_bash", "d")]]);
    }

    #[test]
    fn ambient_and_awaited_entries_are_dropped_but_ambient_false_is_kept() {
        let line = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a","task_type":"t","description":"a","ambient":true},{"task_id":"b","task_type":"t","description":"b","awaited":true},{"task_id":"c","task_type":"t","description":"c","ambient":false}]}"#;
        assert_eq!(jobs_of(line), vec![vec![job("c", "t", "c")]]);
    }

    #[test]
    fn a_sub_agent_scoped_task_list_produces_no_event() {
        let line = r#"{"type":"system","subtype":"background_tasks_changed","parent_tool_use_id":"toolu_x","tasks":[{"task_id":"a","task_type":"t","description":"a"}]}"#;
        assert!(jobs_of(line).is_empty());
    }

    #[test]
    fn an_entry_without_type_or_description_gets_defaults() {
        let line =
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"abc"}]}"#;
        assert_eq!(jobs_of(line), vec![vec![job("abc", "unknown", "abc")]]);
    }

    #[test]
    fn background_jobs_persist_as_a_session_note() {
        let event = AgentEvent::BackgroundJobs {
            running: vec![
                job("a", "local_bash", "first"),
                job("b", "local_agent", "second"),
            ],
        };
        assert_eq!(event.event_type(), EventType::SessionNote);
        let payload = event.payload();
        assert_eq!(payload["kind"], "background_jobs");
        assert_eq!(
            payload["message"],
            "2 background job(s) running: first; second"
        );
        assert_eq!(
            payload["running"],
            json!([
                {"id": "a", "kind": "local_bash", "description": "first"},
                {"id": "b", "kind": "local_agent", "description": "second"},
            ])
        );
        let none = AgentEvent::BackgroundJobs { running: vec![] }.payload();
        assert_eq!(none["message"], "no background jobs running");
    }

    // ---- claude_settings and the settings check (#346) -------------------

    fn settings_of(yaml: &str) -> ClaudeSettings {
        ClaudeSettings::from_yaml(Some(serde_yaml::from_str(yaml).unwrap())).unwrap()
    }

    fn sentinel_regexes() -> (regex::Regex, regex::Regex) {
        (
            regex::Regex::new("^CHOCOFACTORY_SETTINGS_[0-9a-f]{32}$").unwrap(),
            regex::Regex::new("^[0-9a-f]{32}$").unwrap(),
        )
    }

    /// The path of the real `choco` next to this test binary (built by
    /// `cargo build --workspace --all-targets`). A missing one fails the test.
    fn real_choco() -> String {
        let mut path = std::env::current_exe().unwrap();
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
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn isolated_settings_with_no_settings_and_no_sentinel_is_todays_literal() {
        let cwd = std::env::temp_dir();
        let (json_str, _) =
            isolated_settings("coder", &cwd, &ClaudeSettings::default(), None).unwrap();
        let canon = std::fs::canonicalize(&cwd).unwrap();
        let glob = serde_json::to_string(&format!(
            "!{}/**",
            glob_escape(canon.to_str().unwrap()).trim_end_matches('/')
        ))
        .unwrap();
        assert_eq!(
            json_str,
            format!(
                r#"{{"claudeMdExcludes":[{glob}],"pluginConfigs":{{"cc-plugin-agents-md@builtin":{{"options":{{"instructionFiles":"claude-md-and-agents-md"}}}}}}}}"#
            )
        );
    }

    #[test]
    fn choco_scope_keys_are_written_last_and_other_plugin_entries_survive() {
        let settings = settings_of(
            "claudeMdExcludes: ['!/**']\npluginConfigs:\n  cc-plugin-agents-md@builtin: {options: {instructionFiles: claude-md-only}}\n  other@mkt: {options: {x: 1}}\nattribution: {commit: ''}",
        );
        let (json_str, _) =
            isolated_settings("coder", &std::env::temp_dir(), &settings, None).unwrap();
        let got: Value = serde_json::from_str(&json_str).unwrap();
        let mut expected = expected_settings(&std::env::temp_dir());
        expected["pluginConfigs"]["other@mkt"] = json!({"options": {"x": 1}});
        expected["attribution"] = json!({"commit": ""});
        assert_eq!(got, expected);
    }

    #[test]
    fn a_plugin_configs_that_is_not_a_mapping_is_replaced_by_chocos() {
        let settings = settings_of("pluginConfigs: [x]");
        let (json_str, _) =
            isolated_settings("coder", &std::env::temp_dir(), &settings, None).unwrap();
        let got: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(got, expected_settings(&std::env::temp_dir()));
    }

    #[test]
    fn the_sentinel_goes_into_the_roles_env_mapping_or_a_new_one() {
        let cwd = std::env::temp_dir();
        let with_env = settings_of("env: {VISIBLE: v-1}");
        let (json_str, _) = isolated_settings(
            "coder",
            &cwd,
            &with_env,
            Some(("CHOCOFACTORY_SETTINGS_aa", "bb")),
        )
        .unwrap();
        let got: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(
            got["env"],
            json!({"VISIBLE": "v-1", "CHOCOFACTORY_SETTINGS_aa": "bb"})
        );
        let without = settings_of("attribution: {commit: ''}");
        let (json_str, _) = isolated_settings(
            "coder",
            &cwd,
            &without,
            Some(("CHOCOFACTORY_SETTINGS_aa", "bb")),
        )
        .unwrap();
        let got: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(got["env"], json!({"CHOCOFACTORY_SETTINGS_aa": "bb"}));
    }

    #[test]
    fn a_role_env_that_is_not_a_mapping_is_an_error_never_replaced() {
        for yaml in ["env: [a]", "env: env-secret-x", "env: 3", "env: true"] {
            let err = isolated_settings(
                "coder",
                &std::env::temp_dir(),
                &settings_of(yaml),
                Some(("CHOCOFACTORY_SETTINGS_aa", "bb")),
            )
            .unwrap_err();
            assert_eq!(
                err,
                "role 'coder': claude_settings 'env' must be a mapping, because choco adds a variable to it to check that Claude Code applied the settings"
            );
        }
    }

    #[test]
    fn validate_role_refuses_a_non_mapping_env_only_for_an_isolated_role() {
        let adapter = ClaudeAdapter::with_binary("claude");
        let isolated = Isolation::default();
        let message = "role 'coder': claude_settings 'env' must be a mapping, because choco adds a variable to it to check that Claude Code applied the settings";
        for yaml in ["env: [a]", "env: x", "env: 1", "env: false"] {
            assert_eq!(
                adapter
                    .validate_role("coder", &isolated, &settings_of(yaml))
                    .unwrap_err(),
                message,
                "{yaml}"
            );
            assert!(
                adapter
                    .validate_role(
                        "chat",
                        &Isolation::InheritOperatorConfig,
                        &settings_of(yaml)
                    )
                    .is_ok(),
                "chat takes any env: {yaml}"
            );
        }
        for yaml in ["env: {A: b}", "env: ~", "attribution: {commit: ''}", "{}"] {
            assert!(
                adapter
                    .validate_role("coder", &isolated, &settings_of(yaml))
                    .is_ok(),
                "{yaml}"
            );
        }
    }

    fn status_reply(servers: Value) -> Value {
        json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "chocofactory-mcp-status",
            "response": {"mcpServers": servers}}})
    }

    fn server(status: &str, tools: &[&str]) -> Value {
        json!({"name": "chocofactory", "status": status,
               "config": {"args": ["--claude-settings-sentinel", "CHOCOFACTORY_SETTINGS_aa=secret-value"]},
               "tools": tools.iter().map(|t| json!({"name": t})).collect::<Vec<_>>()})
    }

    #[test]
    fn the_mcp_status_verdict_passes_only_a_connected_server_offering_report_outcome() {
        assert_eq!(
            settings_verdict(&status_reply(json!([server(
                "connected",
                &["report_outcome"]
            )]))),
            Ok(())
        );
        let unconfirmed = |status: &str| {
            Err(SettingsVerdict::Unconfirmed {
                status: status.to_string(),
            })
        };
        assert_eq!(
            settings_verdict(&status_reply(json!([server(
                "connected",
                &["claude_settings_rejected"]
            )]))),
            Err(SettingsVerdict::Rejected)
        );
        assert_eq!(
            settings_verdict(&status_reply(json!([server("failed", &[])]))),
            unconfirmed("failed")
        );
        assert_eq!(
            settings_verdict(&status_reply(json!([server("pending", &[])]))),
            unconfirmed("pending")
        );
        assert_eq!(
            settings_verdict(&status_reply(json!([server("connected", &["other"])]))),
            unconfirmed("connected without report_outcome")
        );
        assert_eq!(
            settings_verdict(&status_reply(
                json!([{"name": "other", "status": "connected"}])
            )),
            unconfirmed("not listed")
        );
        assert_eq!(
            settings_verdict(&status_reply(json!([]))),
            unconfirmed("not listed")
        );
        assert_eq!(
            settings_verdict(&json!({"type": "control_response", "response": {
                "subtype": "error", "request_id": "chocofactory-mcp-status",
                "error": "secret-error-text"}})),
            unconfirmed("the CLI refused the mcp_status request")
        );
        assert_eq!(
            settings_verdict(&json!({"type": "control_response"})),
            unconfirmed("the CLI refused the mcp_status request")
        );
        // A status word that is not plain lowercase is never copied.
        assert_eq!(
            settings_verdict(&status_reply(json!([server(
                "CHOCOFACTORY_SETTINGS_aa=secret-value",
                &[]
            )]))),
            unconfirmed("unrecognised status")
        );
    }

    #[test]
    fn the_rejected_event_message_names_the_role_and_no_value() {
        let event = AgentEvent::SettingsNotApplied {
            verdict: SettingsVerdict::Rejected,
            role: "coder".to_string(),
        };
        assert_eq!(event.event_type(), EventType::Error);
        assert_eq!(
            event.payload(),
            json!({"message": "Claude Code did not apply role 'coder''s claude_settings: it ignores the whole settings when one value is invalid. choco stopped the turn."})
        );
        let event = AgentEvent::SettingsNotApplied {
            verdict: SettingsVerdict::Unconfirmed {
                status: "failed".to_string(),
            },
            role: "coder".to_string(),
        };
        assert_eq!(
            event.payload(),
            json!({"message": "choco could not confirm that Claude Code applied role 'coder''s claude_settings (its tool server reported: failed). choco stopped the turn."})
        );
    }

    /// Runs the echo fixture with these settings through `start`.
    async fn echo_args_settings(
        isolation: Isolation,
        claude_settings: ClaudeSettings,
    ) -> HashMap<String, String> {
        let adapter = ClaudeAdapter::with_binary(fixture_binary("fake_claude_echo_args.py"))
            .with_choco_binary(real_choco());
        let cfg = RoleConfig {
            claude_settings,
            role: "coder".to_string(),
            project_id: None,
            disallowed_tools: Vec::new(),
            cwd: std::env::temp_dir(),
            model: None,
            system_prompt: None,
            sandboxed: true,
            report_outcomes: vec!["done".to_string()],
            report_sections: Vec::new(),
            isolation,
        };
        let mut handle = adapter.start("go", &cfg).unwrap();
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            next_assistant_message(&mut handle),
        )
        .await
        .expect("the fixture never replied: its prompt was held back");
        let AgentEvent::AssistantMessage { text } = reply else {
            unreachable!()
        };
        echo_fields(&text)
    }

    fn mcp_args_of(fields: &HashMap<String, String>) -> Vec<String> {
        let config: Value = serde_json::from_str(&fields["mcp_config"]).unwrap();
        assert!(
            config["mcpServers"]["chocofactory"].get("env").is_none(),
            "the MCP config must not carry an env: {config}"
        );
        config["mcpServers"]["chocofactory"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn empty_settings_leave_an_isolated_spawn_byte_for_byte_as_before() {
        let fields = echo_args_settings(Isolation::default(), ClaudeSettings::default()).await;
        let canon = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let glob = serde_json::to_string(&format!(
            "!{}/**",
            glob_escape(canon.to_str().unwrap()).trim_end_matches('/')
        ))
        .unwrap();
        assert_eq!(
            fields["settings"],
            format!(
                r#"{{"claudeMdExcludes":[{glob}],"pluginConfigs":{{"cc-plugin-agents-md@builtin":{{"options":{{"instructionFiles":"claude-md-and-agents-md"}}}}}}}}"#
            )
        );
        assert!(
            !mcp_args_of(&fields)
                .iter()
                .any(|a| a.contains("claude-settings-sentinel")),
            "no sentinel without settings"
        );
        // The writer sent only `initialize`: no `mcp_status` is asked.
        let initialize: Value = serde_json::from_str(&fields["initialize"]).unwrap();
        assert_eq!(initialize, json!({ "subtype": "initialize", "skills": [] }));
    }

    #[tokio::test]
    async fn an_isolated_spawn_with_settings_adds_exactly_one_sentinel_to_env_and_to_the_tool_servers_args()
     {
        let settings = settings_of("attribution: {commit: c1}\nenv: {VISIBLE: v-1}");
        let fields = echo_args_settings(Isolation::default(), settings).await;
        assert_eq!(fields["settings_count"], "1");
        let sent: Value = serde_json::from_str(&fields["settings"]).unwrap();
        assert_eq!(sent["attribution"], json!({"commit": "c1"}));
        let env = sent["env"].as_object().unwrap();
        let (name_re, value_re) = sentinel_regexes();
        let sentinels: Vec<_> = env.iter().filter(|(k, _)| name_re.is_match(k)).collect();
        assert_eq!(sentinels.len(), 1, "{env:?}");
        assert_eq!(
            env.len(),
            2,
            "the role's key plus exactly one sentinel: {env:?}"
        );
        assert_eq!(env["VISIBLE"], "v-1");
        let (name, value) = sentinels[0];
        assert!(value_re.is_match(value.as_str().unwrap()));
        // The expected pair reaches the tool server as an argument, from the
        // same variables.
        let args = mcp_args_of(&fields);
        let at = args
            .iter()
            .position(|a| a == "--claude-settings-sentinel")
            .expect("the sentinel flag");
        assert_eq!(args[at + 1], format!("{name}={}", value.as_str().unwrap()));
        // Claude's own environment never carries it.
        assert!(!fields["mcp_config"].contains("\"env\""));
    }

    /// D2 through the spawn: config.yaml `env` + `attribution`, the workflow's
    /// `env` replaces the whole `env` (no deep merge), and a workflow `null`
    /// leaves `env` holding only the sentinel.
    #[tokio::test]
    async fn layered_settings_reach_the_spawn_key_by_key_and_a_null_leaves_only_the_sentinel() {
        use crate::global_config::{GlobalConfig, GlobalRoleConfig};
        let mut global = GlobalConfig::default();
        global.roles.insert(
            "coder".to_string(),
            GlobalRoleConfig {
                claude_settings: settings_of(
                    "env: {A: from-global}\nattribution: {commit: global-c}",
                ),
                ..Default::default()
            },
        );
        let def = |yaml: &str| crate::workflow_def::RoleDef {
            claude_settings: settings_of(yaml),
            cli: Some("claude".to_string()),
            model: None,
            system_prompt_file: None,
            isolation: Isolation::default(),
            disallowed_tools: Vec::new(),
            read_only: false,
        };
        let (name_re, _) = sentinel_regexes();

        let merged = crate::role_config::resolve_claude_settings(
            "coder",
            &def("env: {B: from-workflow}"),
            &global,
        )
        .unwrap();
        let fields = echo_args_settings(Isolation::default(), merged).await;
        let sent: Value = serde_json::from_str(&fields["settings"]).unwrap();
        assert_eq!(sent["attribution"], json!({"commit": "global-c"}));
        let env = sent["env"].as_object().unwrap();
        assert_eq!(env["B"], "from-workflow");
        assert!(!env.contains_key("A"), "no deep merge: {env:?}");
        assert_eq!(env.keys().filter(|k| name_re.is_match(k)).count(), 1);
        assert_eq!(env.len(), 2);

        let merged =
            crate::role_config::resolve_claude_settings("coder", &def("env: ~"), &global).unwrap();
        let fields = echo_args_settings(Isolation::default(), merged).await;
        let sent: Value = serde_json::from_str(&fields["settings"]).unwrap();
        assert_eq!(sent["attribution"], json!({"commit": "global-c"}));
        let env = sent["env"].as_object().unwrap();
        assert_eq!(env.len(), 1, "only the sentinel: {env:?}");
        assert!(name_re.is_match(env.keys().next().unwrap()));
    }

    #[tokio::test]
    async fn a_chat_spawn_passes_its_settings_once_alone_and_with_no_sentinel() {
        let settings = settings_of("attribution: {commit: ''}");
        let fields = echo_args_settings(Isolation::InheritOperatorConfig, settings).await;
        assert_eq!(fields["settings_count"], "1");
        let sent: Value = serde_json::from_str(&fields["settings"]).unwrap();
        assert_eq!(sent, json!({"attribution": {"commit": ""}}));
        assert!(
            !mcp_args_of(&fields)
                .iter()
                .any(|a| a.contains("claude-settings-sentinel"))
        );
        for empty in [ClaudeSettings::default(), settings_of("{}")] {
            let fields = echo_args_settings(Isolation::InheritOperatorConfig, empty).await;
            assert_eq!(fields["settings"], "<unset>");
            assert_eq!(fields["settings_count"], "0");
        }
    }

    #[tokio::test]
    async fn two_spawns_get_different_sentinels() {
        let settings = settings_of("attribution: {commit: ''}");
        let a = echo_args_settings(Isolation::default(), settings.clone()).await;
        let b = echo_args_settings(Isolation::default(), settings).await;
        let sentinel = |fields: &HashMap<String, String>| {
            let args = mcp_args_of(fields);
            let at = args
                .iter()
                .position(|a| a == "--claude-settings-sentinel")
                .unwrap();
            args[at + 1].clone()
        };
        assert_ne!(sentinel(&a), sentinel(&b));
    }

    /// A temp dir holding a `/bin/sh` wrapper around the fake claude, with the
    /// fixture's mode set by the wrapper, never by the test process.
    struct Gate {
        dir: PathBuf,
        wrapper: String,
    }

    impl Gate {
        fn new(mode: &str, init_tools: Option<&str>) -> Self {
            let dir = std::env::temp_dir().join(format!("choco-gate-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let wrapper = dir.join("claude");
            let init = init_tools
                .map(|tools| format!("export FAKE_CLAUDE_INIT_TOOLS='{tools}'\n"))
                .unwrap_or_default();
            crate::test_support::write_executable(
                &wrapper,
                format!(
                    "#!/bin/sh\nexport FAKE_CLAUDE_SETTINGS={mode}\nexport FAKE_CLAUDE_PROMPT_MARKER={dir}/prompt\nexport FAKE_CLAUDE_ARGV_FILE={dir}/argv.json\n{init}exec {fixture} \"$@\"\n",
                    dir = dir.display(),
                    fixture = fixture_binary("fake_claude.py"),
                ),
            );
            Gate {
                dir,
                wrapper: wrapper.to_string_lossy().into_owned(),
            }
        }

        fn prompt_written(&self) -> bool {
            self.dir.join("prompt").exists()
        }

        /// Starts the single-shot turn and returns every event until the
        /// stream ends.
        async fn run(&self, choco: &str) -> Vec<AgentEvent> {
            let adapter = ClaudeAdapter::with_binary(self.wrapper.clone()).with_choco_binary(choco);
            let cfg = RoleConfig {
                claude_settings: settings_of("attribution: {commit: secret-commit-text}"),
                role: "coder".to_string(),
                project_id: None,
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
            let mut events = Vec::new();
            let result = tokio::time::timeout(crate::test_support::LOAD_ALLOWANCE, async {
                while let Some(event) = handle.recv().await {
                    // As the session manager does once a turn has ended.
                    if matches!(event, AgentEvent::TurnCompleted { .. }) {
                        handle.close_stdin();
                    }
                    events.push(event);
                }
            })
            .await;
            assert!(result.is_ok(), "the stream did not end: {events:?}");
            let status = handle.wait().await.unwrap();
            assert!(status.success(), "the fake exits cleanly on stdin EOF");
            events
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn not_applied(events: &[AgentEvent]) -> Vec<&SettingsVerdict> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::SettingsNotApplied { verdict, role } => {
                    assert_eq!(role, "coder");
                    Some(verdict)
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn applied_settings_open_the_gate_and_the_turn_completes() {
        let gate = Gate::new("apply", None);
        let events = gate.run(&real_choco()).await;
        assert!(not_applied(&events).is_empty(), "{events:?}");
        assert!(gate.prompt_written());
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnCompleted {
                is_error: false,
                ..
            }
        )));
        let meta = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::SessionMeta { details, .. } => Some(details),
                _ => None,
            })
            .unwrap();
        assert_eq!(meta["claude_settings_keys"], json!(["attribution"]));
        // Key names only: no value reaches an event.
        let serialized = format!("{events:?}");
        assert!(!serialized.contains("secret-commit-text"), "{serialized}");
        // Nothing in any event carries the sentinel.
        let argv: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(gate.dir.join("argv.json")).unwrap())
                .unwrap();
        let config: Value =
            serde_json::from_str(&argv[argv.iter().position(|a| a == "--mcp-config").unwrap() + 1])
                .unwrap();
        let args = config["mcpServers"]["chocofactory"]["args"]
            .as_array()
            .unwrap();
        let flag = args[args
            .iter()
            .position(|a| a == "--claude-settings-sentinel")
            .unwrap()
            + 1]
        .as_str()
        .unwrap();
        let (name, value) = flag.split_once('=').unwrap();
        let all = format!("{events:?}");
        assert!(!all.contains(name) && !all.contains(value), "{all}");
    }

    #[tokio::test]
    async fn rejected_settings_stop_the_turn_before_its_prompt() {
        let gate = Gate::new("reject", None);
        let events = gate.run(&real_choco()).await;
        assert_eq!(not_applied(&events), vec![&SettingsVerdict::Rejected]);
        assert!(!gate.prompt_written(), "the prompt must never be written");
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AgentEvent::TurnCompleted { .. } | AgentEvent::AssistantMessage { .. }
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_cli_that_refuses_mcp_status_is_unconfirmed_and_gets_no_prompt() {
        let gate = Gate::new("refuse", None);
        let events = gate.run(&real_choco()).await;
        assert_eq!(
            not_applied(&events),
            vec![&SettingsVerdict::Unconfirmed {
                status: "the CLI refused the mcp_status request".to_string()
            }]
        );
        assert!(!gate.prompt_written());
        // The refusal's own text is not turned into a second error event.
        assert!(
            !events.iter().any(
                |e| matches!(e, AgentEvent::Error { message } if message.contains("Unsupported"))
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_tool_server_is_unconfirmed_and_gets_no_prompt() {
        let gate = Gate::new("apply", None);
        let events = gate.run("/usr/bin/false").await;
        assert_eq!(
            not_applied(&events),
            vec![&SettingsVerdict::Unconfirmed {
                status: "failed".to_string()
            }]
        );
        assert!(!gate.prompt_written());
    }

    #[tokio::test]
    async fn an_init_line_listing_the_rejected_tool_is_a_second_detector() {
        let gate = Gate::new(
            "apply",
            Some(r#"["mcp__chocofactory__claude_settings_rejected"]"#),
        );
        let events = gate.run(&real_choco()).await;
        assert_eq!(not_applied(&events), vec![&SettingsVerdict::Rejected]);
        assert!(!gate.prompt_written());
    }

    #[tokio::test]
    async fn a_cli_that_never_answers_holds_the_prompt_back() {
        let gate = Gate::new("silent", None);
        let adapter =
            ClaudeAdapter::with_binary(gate.wrapper.clone()).with_choco_binary(real_choco());
        let cfg = RoleConfig {
            claude_settings: settings_of("attribution: {commit: c1}"),
            role: "coder".to_string(),
            project_id: None,
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
        handle.send("a later message").unwrap();
        // The init line arrives; nothing else, and no prompt reaches the CLI.
        let first = tokio::time::timeout(crate::test_support::LOAD_ALLOWANCE, handle.recv())
            .await
            .unwrap();
        assert!(matches!(first, Some(AgentEvent::SessionMeta { .. })));
        let more = tokio::time::timeout(std::time::Duration::from_millis(700), handle.recv()).await;
        assert!(more.is_err(), "nothing more is expected: {more:?}");
        assert!(!gate.prompt_written());
    }
}
