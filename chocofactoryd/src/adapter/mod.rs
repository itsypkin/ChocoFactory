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
        session_id: String,
        details: Value,
    },
    Error {
        message: String,
    },
    /// The turn was cut off from outside: the CLI reported that the account
    /// hit a usage/rate limit (#92). Recorded as an ordinary `Error` on the
    /// timeline — it *is* an error for the turn — but carried as its own
    /// variant so `session::drain_session` can end the run
    /// `TaskRunEndReason::Interrupted` and `retry` can resume the session
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
                session_id,
                details,
            } => {
                let mut payload = serde_json::json!({ "session_id": session_id });
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
            AgentEvent::TurnCompleted { is_error } => {
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
    fn start(&self, prompt: &str, cfg: &RoleConfig) -> Result<AgentHandle, AdapterError>;
    fn resume(
        &self,
        session_id: &str,
        prompt: &str,
        cfg: &RoleConfig,
    ) -> Result<AgentHandle, AdapterError>;
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
