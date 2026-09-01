pub mod claude;

use std::fmt;
use std::path::PathBuf;

use chokofactory_core::models::EventType;
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
}

/// The shared, CLI-agnostic event shape (design §4.2). Carries the same
/// information as `chokofactory_core::models::EventType` + payload, so
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
    SessionMeta {
        session_id: String,
    },
    Error {
        message: String,
    },
    /// The CLI's `result` line: this turn is over and the process is now
    /// only waiting on stdin EOF to exit — it never exits on its own (#70).
    /// `is_error` mirrors the `result` message's own flag, so a caller that
    /// only wants to treat a *clean* finish as completion (§5.2, a
    /// single-shot `agent_turn`) doesn't have to re-inspect the raw JSON.
    TurnCompleted {
        is_error: bool,
    },
}

impl AgentEvent {
    pub fn event_type(&self) -> EventType {
        match self {
            AgentEvent::AssistantMessage { .. } => EventType::AssistantMessage,
            AgentEvent::ToolCall { .. } => EventType::ToolCall,
            AgentEvent::ToolResult { .. } => EventType::ToolResult,
            AgentEvent::Thinking { .. } => EventType::Thinking,
            AgentEvent::SessionMeta { .. } => EventType::SessionMeta,
            AgentEvent::Error { .. } => EventType::Error,
            AgentEvent::TurnCompleted { .. } => EventType::TurnCompleted,
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
            AgentEvent::SessionMeta { session_id } => {
                serde_json::json!({ "session_id": session_id })
            }
            AgentEvent::Error { message } => serde_json::json!({ "message": message }),
            AgentEvent::TurnCompleted { is_error } => {
                serde_json::json!({ "is_error": is_error })
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
