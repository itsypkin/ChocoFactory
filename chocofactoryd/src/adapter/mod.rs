pub mod claude;

use std::fmt;
use std::path::PathBuf;

use chocofactory_core::models::EventType;
use serde_json::Value;
use tokio::sync::mpsc;

pub use claude::ClaudeAdapter;

/// Per-role settings an adapter needs to spawn its CLI (§4, §5.5's role
/// config resolution). `system_prompt` is already-resolved text — reading
/// a workflow definition's `system_prompt_file` is the caller's job, not
/// the adapter's.
#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    /// Whether `cwd` is a disposable, isolated working copy the workflow
    /// definition opted into (`worktree: true`, §5.5 Q7, issue #58) rather
    /// than the task's real configured repo (or the daemon's own cwd, for
    /// a workflow like `chat` that has no repo at all). An adapter that
    /// bypasses its CLI's own per-edit permission prompts (#67) must only
    /// do so when this is `true` — the disposable worktree *is* the
    /// sandbox that makes bypassing safe; without it, bypassing would
    /// apply unconditionally to a real, non-disposable checkout.
    pub sandboxed: bool,
    /// The current stage's `on:` edge names (issue #73), i.e. the outcomes
    /// this turn is allowed to report through `choco mcp-serve`'s
    /// `report_outcome` tool. Empty when the stage declares no edges (a
    /// standing session like chat, or a plain `agent_turn` that always
    /// advances on `done`) — the tool is still offered, but as an optional,
    /// free-form status report rather than a routing decision. Stage-derived,
    /// like `sandboxed`, so it is passed straight through by
    /// `role_config::resolve` rather than layered from config.
    pub report_outcomes: Vec<String>,
    /// The sections this stage requires the turn's report to carry (issue
    /// #95), from the stage's `report_sections:`. Empty — every stage that
    /// hasn't opted in — leaves the report unchecked beyond its `outcome`.
    /// Stage-derived and passed straight through, exactly like
    /// `report_outcomes`.
    pub report_sections: Vec<String>,
    /// How much of the operator's own CLI setup this turn inherits (#90).
    /// Role-derived, from the workflow definition only — never task config
    /// or global config — because every setting here can only *loosen* what
    /// a workflow agent is exposed to.
    pub isolation: Isolation,
    /// Tools this role must not use (#172), from the workflow definition
    /// only — never task or global config. Adapter-neutral names; each
    /// adapter maps them onto its own CLI's tool names.
    pub disallowed_tools: Vec<RoleTool>,
}

/// The adapter-neutral tool vocabulary a workflow's `disallowed_tools` may
/// use (#172). It grows when a use needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleTool {
    Edit,
    Write,
    NotebookEdit,
}

impl RoleTool {
    pub const ALL: [RoleTool; 3] = [RoleTool::Edit, RoleTool::Write, RoleTool::NotebookEdit];

    /// Exact lowercase names only.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "edit" => Some(RoleTool::Edit),
            "write" => Some(RoleTool::Write),
            "notebook_edit" => Some(RoleTool::NotebookEdit),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RoleTool::Edit => "edit",
            RoleTool::Write => "write",
            RoleTool::NotebookEdit => "notebook_edit",
        }
    }
}

/// What a workflow agent's CLI process is allowed to pick up from the
/// operator's machine (#90).
///
/// Left alone, `claude` loads the operator's `~/.claude/CLAUDE.md`, user
/// plugins (with their agents, skills, hooks and MCP tools), output style,
/// MCP servers and auto-memory into every turn. In #88 that steered a coder
/// into delegating its whole job to a background sub-agent and ending its
/// turn with nothing done, and in #61 it narrowed a reviewer's risk list to
/// whatever the operator's memory happened to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Isolation {
    /// The turn runs with the operator's full setup, as every turn did
    /// before #90. Meant for a conversational role like `chat`, where the
    /// operator is the one talking to the agent.
    InheritOperatorConfig,
    /// Only the task repo's own settings and `CLAUDE.md`, only the daemon's
    /// MCP server, no `ReportFindings`, and no auto-memory unless `memory`.
    Isolated {
        /// The skills this turn may invoke. Empty means none: the `Skill`
        /// tool itself is removed.
        skills: Vec<String>,
        /// Whether the turn may read and write the operator's auto-memory.
        memory: bool,
    },
}

// Auto-memory (`CLAUDE_CODE_DISABLE_AUTO_MEMORY`) follows only the variant
// above, never the daemon's own environment (#105): `Isolated { memory:
// false }` sets it, `Isolated { memory: true }` and `InheritOperatorConfig`
// both explicitly remove it. See `claude::apply_auto_memory_env` for why
// removal — not "leave it alone" — is required for the latter two: a value
// already in the daemon's environment is almost always an artifact of where
// the daemon was launched, not a choice made for this role.

impl Default for Isolation {
    /// Isolated, with no skills and no memory: the setting a role gets when
    /// its workflow definition says nothing.
    fn default() -> Self {
        Isolation::Isolated {
            skills: Vec::new(),
            memory: false,
        }
    }
}

impl Isolation {
    /// The isolation this turn actually ran with, for `session_meta` — the
    /// CLI's own `init` line lists every installed skill regardless of the
    /// session allowlist, so the allowlist has to be recorded from our side.
    pub fn describe(&self) -> Value {
        match self {
            Isolation::InheritOperatorConfig => {
                serde_json::json!({ "inherit_operator_config": true })
            }
            Isolation::Isolated { skills, memory } => serde_json::json!({
                "inherit_operator_config": false,
                "skills": skills,
                "memory": memory,
            }),
        }
    }
}

/// The shared, CLI-agnostic event shape (design §4.2). Carries the same
/// information as `chocofactory_core::models::EventType` + payload, so
/// callers can persist it via `events::append` without knowing anything
/// about the adapter that produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    AssistantMessage {
        text: String,
    },
    ToolCall {
        tool_use_id: String,
        tool: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        tool: String,
        output: String,
        is_error: bool,
    },
    Thinking {
        text: String,
    },
    /// The CLI's `system/init` line. It arrives once per CLI turn, so a
    /// turn woken by a background job's notification sends another (#90).
    /// `details` is whatever the adapter can say about the session's real
    /// environment (its tools, MCP servers, version, model, and the
    /// isolation it was launched with), merged into the persisted payload.
    SessionMeta {
        adapter_session_id: String,
        details: Value,
    },
    Error {
        message: String,
    },
    /// The turn was cut off from outside: the CLI reported that the account
    /// hit a usage/rate limit (#92). Recorded as an ordinary `Error` on the
    /// timeline — it *is* an error for the turn — but carried as its own
    /// variant so `session::drain_session` can end the session
    /// `SessionEndReason::Interrupted` and `retry` can resume the session
    /// rather than starting a fresh one over a worktree full of work.
    ///
    /// `detected_by` names the rule that fired, and is persisted, because
    /// one of those rules matches the CLI's message text (see
    /// `adapter::claude::usage_limit_text`): a timeline that says which rule
    /// fired is what makes the brittle one safe to delete once the
    /// structured markers are confirmed against a real session.
    Interrupted {
        message: String,
        detected_by: InterruptionEvidence,
    },
    /// The CLI's `result` line: this turn is over and the process is now
    /// only waiting on stdin EOF to exit — it never exits on its own (#70).
    /// `is_error` mirrors the `result` message's own flag, so a caller that
    /// only wants to treat a *clean* finish as completion (§5.2, a
    /// single-shot `agent_turn`) doesn't have to re-inspect the raw JSON.
    TurnCompleted {
        is_error: bool,
        /// What the turn cost. Every adapter must fill every field: unknown
        /// is `None` / `BillingMode::Unknown`, never zero.
        usage: TurnUsage,
    },
    /// Something a sub-agent did, rather than the main agent (#90). The CLI
    /// streams a sub-agent's tool calls and results on the same stdout as
    /// the main agent's, marked with the id of the `Agent` tool call that
    /// spawned it. Kept distinct so nothing that decides a turn's outcome
    /// (its `report_outcome` call, its final reply) can be taken from a
    /// delegated helper by mistake.
    Subagent {
        parent_tool_use_id: String,
        event: Box<AgentEvent>,
    },
}

/// What one turn used, as its CLI reported it. Part of the adapter
/// contract: every adapter fills every field. There is deliberately no
/// `Default`, so a new adapter cannot compile without saying, field by
/// field, what it knows. Unknown is `None` or `BillingMode::Unknown`,
/// never `0`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnUsage {
    /// Cost in USD; see `counting` for whether it is this turn's or the
    /// conversation's running total.
    pub cost_usd: Option<f64>,
    /// Always per turn.
    pub tokens: TokenCounts,
    /// Per-model breakdown (counted like `cost_usd`). `None` = the adapter
    /// can't say.
    pub models: Option<Vec<ModelUsage>>,
    pub wall_time_ms: Option<u64>,
    pub model_turns: Option<u32>,
    pub billing: BillingMode,
    /// How `cost_usd` and `models` are counted.
    pub counting: UsageCounting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: TokenCounts,
    pub cost_usd: Option<f64>,
}

/// How the turn was paid for, as far as the adapter can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingMode {
    Subscription,
    ApiKey,
    Unknown,
}

impl BillingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BillingMode::Subscription => "subscription",
            BillingMode::ApiKey => "api_key",
            BillingMode::Unknown => "unknown",
        }
    }
}

/// Whether a turn's reported cost and per-model figures are its own or a
/// running total for the whole CLI conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageCounting {
    CumulativePerConversation,
    PerTurn,
}

impl UsageCounting {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageCounting::CumulativePerConversation => "cumulative",
            UsageCounting::PerTurn => "per_turn",
        }
    }
}

/// Which of the claude adapter's rules recognised a usage limit (#92) —
/// see `assistant_interruption`, `normalize_rate_limit_event` and
/// `usage_limit_text` there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptionEvidence {
    /// A field the CLI emits for machines: an assistant message's
    /// `error: "rate_limit"`/`apiErrorStatus: 429`, or a `rate_limit_event`
    /// line whose `rate_limit_info.status` is `rejected`.
    Structured,
    /// The `result` line's own human-readable text. Deliberately last, and
    /// deliberately labelled: the CLI's wording is not an interface, so a
    /// run recognised this way is one to check before trusting.
    MessageText,
}

impl InterruptionEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            InterruptionEvidence::Structured => "structured",
            InterruptionEvidence::MessageText => "message_text",
        }
    }
}

impl AgentEvent {
    pub fn event_type(&self) -> EventType {
        match self {
            AgentEvent::AssistantMessage { .. } => EventType::AssistantMessage,
            AgentEvent::ToolCall { .. } => EventType::ToolCall,
            AgentEvent::ToolResult { .. } => EventType::ToolResult,
            AgentEvent::Thinking { .. } => EventType::Thinking,
            AgentEvent::SessionMeta { .. } => EventType::SessionMeta,
            AgentEvent::Error { .. } | AgentEvent::Interrupted { .. } => EventType::Error,
            AgentEvent::TurnCompleted { .. } => EventType::TurnCompleted,
            AgentEvent::Subagent { event, .. } => event.event_type(),
        }
    }

    pub fn payload(&self) -> Value {
        match self {
            AgentEvent::AssistantMessage { text } => serde_json::json!({ "text": text }),
            AgentEvent::ToolCall {
                tool_use_id,
                tool,
                input,
            } => serde_json::json!({
                "tool_use_id": tool_use_id,
                "tool": tool,
                "input": input,
            }),
            AgentEvent::ToolResult {
                tool_use_id,
                tool,
                output,
                is_error,
            } => serde_json::json!({
                "tool_use_id": tool_use_id,
                "tool": tool,
                "output": output,
                "is_error": is_error,
            }),
            AgentEvent::Thinking { text } => serde_json::json!({ "text": text }),
            AgentEvent::SessionMeta {
                adapter_session_id,
                details,
            } => {
                let mut payload = serde_json::json!({ "adapter_session_id": adapter_session_id });
                if let (Value::Object(payload), Value::Object(details)) = (&mut payload, details) {
                    for (key, value) in details {
                        payload.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
                payload
            }
            AgentEvent::Error { message } => serde_json::json!({ "message": message }),
            AgentEvent::Interrupted {
                message,
                detected_by,
            } => serde_json::json!({
                "message": message,
                "interrupted": "usage_limit",
                "detected_by": detected_by.as_str(),
            }),
            AgentEvent::TurnCompleted { is_error, .. } => {
                serde_json::json!({ "is_error": is_error })
            }
            AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } => {
                let mut payload = event.payload();
                if let Value::Object(map) = &mut payload {
                    map.insert(
                        "parent_tool_use_id".to_string(),
                        Value::String(parent_tool_use_id.clone()),
                    );
                }
                payload
            }
        }
    }
}

#[derive(Debug)]
pub enum AdapterError {
    Spawn(std::io::Error),
    ProcessExited,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdapterError::Spawn(err) => write!(f, "failed to spawn agent process: {err}"),
            AdapterError::ProcessExited => {
                write!(f, "cannot send: agent process has already exited")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

/// A CLI adapter: knows how to start or resume a session for one
/// underlying agentic CLI (`claude`, `codex`, `gemini`, ...) and translate
/// its native output into `AgentEvent`s (§4).
pub trait AgentAdapter: Send + Sync {
    /// The name a role's `cli:` uses to select this adapter, and the value
    /// stored in `sessions.cli_adapter`.
    fn name(&self) -> &'static str;
    fn start(&self, prompt: &str, cfg: &RoleConfig) -> Result<AgentHandle, AdapterError>;
    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
    ) -> Result<AgentHandle, AdapterError>;
}

/// A `cli:` value that names no adapter in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCliError {
    pub role: Option<String>,
    pub cli: String,
    pub known: Vec<&'static str>,
}

impl fmt::Display for UnknownCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let known = self.known.join(", ");
        match &self.role {
            Some(role) => write!(
                f,
                "role '{role}' uses cli '{}', which this daemon doesn't know; known CLIs: {known}",
                self.cli
            ),
            None => write!(
                f,
                "cli '{}' is not one this daemon knows; known CLIs: {known}",
                self.cli
            ),
        }
    }
}

impl std::error::Error for UnknownCliError {}

/// The adapters this daemon can run, keyed by [`AgentAdapter::name`]. The
/// valid `cli:` values are exactly its keys.
#[derive(Clone)]
pub struct Registry {
    adapters: std::collections::BTreeMap<&'static str, std::sync::Arc<dyn AgentAdapter>>,
}

impl Registry {
    /// Panics if two adapters share a name: a programmer error.
    pub fn new(adapters: Vec<std::sync::Arc<dyn AgentAdapter>>) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for adapter in adapters {
            let name = adapter.name();
            if map.insert(name, adapter).is_some() {
                panic!("two adapters registered under the name '{name}'");
            }
        }
        Self { adapters: map }
    }

    pub fn single(adapter: std::sync::Arc<dyn AgentAdapter>) -> Self {
        Self::new(vec![adapter])
    }

    pub fn lookup(
        &self,
        role: Option<&str>,
        cli: &str,
    ) -> Result<&std::sync::Arc<dyn AgentAdapter>, UnknownCliError> {
        self.adapters.get(cli).ok_or_else(|| UnknownCliError {
            role: role.map(str::to_string),
            cli: cli.to_string(),
            known: self.names(),
        })
    }

    /// Sorted adapter names.
    pub fn names(&self) -> Vec<&'static str> {
        self.adapters.keys().copied().collect()
    }
}

/// Checks every string `roles.<name>.cli` in a task config against the
/// registry, in sorted role-name order. Non-object shapes and non-string
/// (or `null`) values are skipped: they fall through to the next config
/// layer, as they always have.
pub fn check_task_config_clis(config: &Value, registry: &Registry) -> Result<(), UnknownCliError> {
    let Some(roles) = config.get("roles").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut names: Vec<&String> = roles.keys().collect();
    names.sort();
    for name in names {
        if let Some(cli) = roles[name].get("cli").and_then(Value::as_str) {
            registry.lookup(Some(name), cli)?;
        }
    }
    Ok(())
}

/// A live (or just-exited) agent subprocess. Streams normalized
/// `AgentEvent`s and accepts further messages over stdin while the
/// process is alive (§4, §4.1's active-state behavior).
pub struct AgentHandle {
    child: tokio::process::Child,
    events_rx: mpsc::UnboundedReceiver<AgentEvent>,
    stdin_tx: mpsc::UnboundedSender<String>,
}

impl AgentHandle {
    pub(crate) fn new(
        child: tokio::process::Child,
        events_rx: mpsc::UnboundedReceiver<AgentEvent>,
        stdin_tx: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            child,
            events_rx,
            stdin_tx,
        }
    }

    /// Waits for the next normalized event. Returns `None` once the
    /// process has exited and every buffered event has been delivered.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.events_rx.recv().await
    }

    /// Feeds another user turn into the live process's stdin.
    pub fn send(&self, text: &str) -> Result<(), AdapterError> {
        self.stdin_tx
            .send(text.to_string())
            .map_err(|_| AdapterError::ProcessExited)
    }

    /// Closes the subprocess's stdin, signaling end-of-input so the CLI
    /// finishes its current turn and exits on its own (§4.1 step 2 — idle
    /// teardown). Does not kill the process; keep draining `recv` until it
    /// returns `None`, then `wait` to reap it.
    pub fn close_stdin(&mut self) {
        let (dummy_tx, _dummy_rx) = mpsc::unbounded_channel();
        self.stdin_tx = dummy_tx;
    }

    /// The process group id to signal when cancelling this session (#69).
    ///
    /// Equal to the child's own pid: adapters spawn with
    /// `Command::process_group(0)`, which makes the child a group leader
    /// whose pgid is its pid. `None` once the process has been reaped.
    ///
    /// Callers must not cache this. It is only safe to signal until
    /// [`Self::wait`] reaps the child, after which the number may already
    /// belong to an unrelated process; the freshness of a stored copy is
    /// the caller's problem, not this method's (see
    /// `SessionSignals::pgid`, which keeps it behind a lock and clears it
    /// before reaping for exactly that reason).
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Waits for the underlying process to exit, reaping it.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

/// The instruction appended to a single-shot turn's system prompt (#90).
///
/// Says what completion *is* rather than only asking for a verdict: a turn
/// that ends without the call is treated as still working (and eventually
/// nudged), which is what lets an agent wait on its own background work
/// without the daemon mistaking that pause for "done".
pub(crate) fn report_instruction(outcomes: &[String]) -> String {
    format!(
        "When all of your work for this stage is finished (including anything you started \
         in the background, which you must wait for), call `report_outcome` to report the \
         stage's outcome. It must be one of: {}. Calling it is how this stage completes: \
         ending your turn without calling it means you are still working. If \
         `report_outcome` is listed as a deferred tool, load it with ToolSearch first.",
        outcomes.join(", ")
    )
}

/// Whether an error message reads like a usage limit rather than a failure
/// the agent caused. Matched case-insensitively against the phrasings seen
/// on a real limit (`You've hit your session limit · resets 3:40pm`) and the
/// API's own wording. Brittle by construction — see `normalize_result`.
pub(crate) fn usage_limit_text(message: &str) -> bool {
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
mod registry_tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::recording_adapter::RecordingAdapter;

    fn registry() -> Registry {
        Registry::new(vec![
            RecordingAdapter::new("zeta", "unused"),
            RecordingAdapter::new("alpha", "unused"),
        ])
    }

    #[test]
    fn lookup_returns_the_adapter_keyed_by_its_own_name() {
        let registry = registry();
        assert_eq!(registry.lookup(None, "zeta").unwrap().name(), "zeta");
        assert_eq!(registry.names(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn a_miss_lists_the_known_names_sorted_and_formats_exactly() {
        let registry = registry();
        let err = registry.lookup(Some("coder"), "cluade").err().unwrap();
        assert_eq!(err.known, vec!["alpha", "zeta"]);
        assert_eq!(
            err.to_string(),
            "role 'coder' uses cli 'cluade', which this daemon doesn't know; known CLIs: alpha, zeta"
        );
        let err = registry.lookup(None, "cluade").err().unwrap();
        assert_eq!(
            err.to_string(),
            "cli 'cluade' is not one this daemon knows; known CLIs: alpha, zeta"
        );
    }

    #[test]
    #[should_panic(expected = "two adapters registered under the name 'dup'")]
    fn two_adapters_with_one_name_is_a_programmer_error() {
        let a: Arc<dyn AgentAdapter> = RecordingAdapter::new("dup", "unused");
        let b: Arc<dyn AgentAdapter> = RecordingAdapter::new("dup", "unused");
        Registry::new(vec![a, b]);
    }

    #[test]
    fn task_config_check_skips_shapes_that_fall_through_and_rejects_unknown_strings() {
        let registry = registry();
        for ok in [
            json!({"roles": {"coder": {"cli": 1}}}),
            json!({"roles": {"coder": {"cli": null}}}),
            json!({"roles": "nope"}),
            json!({"roles": {"coder": "nope"}}),
            json!("not an object"),
            json!({"roles": {"coder": {"cli": "alpha"}}}),
        ] {
            assert!(check_task_config_clis(&ok, &registry).is_ok(), "{ok}");
        }
        // A role the workflow doesn't define is still an invalid value.
        let err = check_task_config_clis(
            &json!({"roles": {"nobody": {"cli": "nope"}, "aaa": {"cli": "zeta"}}}),
            &registry,
        )
        .unwrap_err();
        assert_eq!(err.role.as_deref(), Some("nobody"));
        assert_eq!(err.cli, "nope");
    }
}
