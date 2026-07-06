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

    /// Waits for the underlying process to exit, reaping it.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}
