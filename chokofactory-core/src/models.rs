use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A grouping label for related tasks. Holds no repo/path itself (design §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// UUID (v4), stored as text. Kept as a plain `String` rather than a
    /// `Uuid` type so the generation scheme (e.g. a shorter id) can change
    /// later without touching every struct that carries an id.
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// One unit of work, driven by a workflow definition (design §3, §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    /// Set when this task was spawned via delegation from another task (§6.2).
    pub parent_task_id: Option<String>,
    pub workflow_def: String,
    pub title: String,
    /// Free-form for now (e.g. "open", "closed") — the full set of values
    /// is driven by workflow definitions, not fixed by this crate (§5.4).
    pub status: String,
    pub config: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lifecycle state of an agent subprocess session (design §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Active,
    Idle,
    Exited,
}

impl fmt::Display for TaskRunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TaskRunStatus::Active => "active",
            TaskRunStatus::Idle => "idle",
            TaskRunStatus::Exited => "exited",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTaskRunStatusError(pub String);

impl fmt::Display for ParseTaskRunStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid task run status: {}", self.0)
    }
}

impl std::error::Error for ParseTaskRunStatusError {}

impl FromStr for TaskRunStatus {
    type Err = ParseTaskRunStatusError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(TaskRunStatus::Active),
            "idle" => Ok(TaskRunStatus::Idle),
            "exited" => Ok(TaskRunStatus::Exited),
            other => Err(ParseTaskRunStatusError(other.to_string())),
        }
    }
}

/// One row per underlying agent subprocess session a task has had (§3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub task_id: String,
    pub stage: String,
    pub role: String,
    pub cli_adapter: String,
    pub model: String,
    pub session_id: Option<String>,
    pub status: TaskRunStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

/// Normalized event kind emitted by an agent adapter (design §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    AssistantMessage,
    ToolCall,
    ToolResult,
    Thinking,
    Error,
    SessionMeta,
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EventType::AssistantMessage => "assistant_message",
            EventType::ToolCall => "tool_call",
            EventType::ToolResult => "tool_result",
            EventType::Thinking => "thinking",
            EventType::Error => "error",
            EventType::SessionMeta => "session_meta",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseEventTypeError(pub String);

impl fmt::Display for ParseEventTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid event type: {}", self.0)
    }
}

impl std::error::Error for ParseEventTypeError {}

impl FromStr for EventType {
    type Err = ParseEventTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "assistant_message" => Ok(EventType::AssistantMessage),
            "tool_call" => Ok(EventType::ToolCall),
            "tool_result" => Ok(EventType::ToolResult),
            "thinking" => Ok(EventType::Thinking),
            "error" => Ok(EventType::Error),
            "session_meta" => Ok(EventType::SessionMeta),
            other => Err(ParseEventTypeError(other.to_string())),
        }
    }
}

/// Append-only normalized event log row (§3, §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub task_run_id: String,
    pub seq: i64,
    pub event_type: EventType,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

/// Generic workflow-engine bookkeeping for a task, one row per task (§3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub task_id: String,
    pub current_stage: String,
    /// JSON object mapping stage name -> loop count (§5.3).
    pub loop_counters: Value,
    /// JSON array trail of past stage transitions.
    pub stage_history: Value,
    /// Stage-specific data (e.g. PR URL, last check status) owned by
    /// whichever stage kind is currently active.
    pub payload: Value,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_run_status_round_trips_through_display_and_from_str() {
        for status in [
            TaskRunStatus::Active,
            TaskRunStatus::Idle,
            TaskRunStatus::Exited,
        ] {
            assert_eq!(status.to_string().parse::<TaskRunStatus>().unwrap(), status);
        }
    }

    #[test]
    fn task_run_status_from_str_rejects_unknown_value() {
        let err = "bogus".parse::<TaskRunStatus>().unwrap_err();
        assert_eq!(err.0, "bogus");
        assert_eq!(err.to_string(), "invalid task run status: bogus");
    }

    #[test]
    fn event_type_round_trips_through_display_and_from_str() {
        for event_type in [
            EventType::AssistantMessage,
            EventType::ToolCall,
            EventType::ToolResult,
            EventType::Thinking,
            EventType::Error,
            EventType::SessionMeta,
        ] {
            assert_eq!(
                event_type.to_string().parse::<EventType>().unwrap(),
                event_type
            );
        }
    }

    #[test]
    fn event_type_from_str_rejects_unknown_value() {
        let err = "bogus".parse::<EventType>().unwrap_err();
        assert_eq!(err.0, "bogus");
        assert_eq!(err.to_string(), "invalid event type: bogus");
    }

    #[test]
    fn task_run_status_serializes_to_snake_case_json() {
        assert_eq!(
            serde_json::to_string(&TaskRunStatus::Idle).unwrap(),
            "\"idle\""
        );
    }

    #[test]
    fn event_type_serializes_to_snake_case_json() {
        assert_eq!(
            serde_json::to_string(&EventType::ToolResult).unwrap(),
            "\"tool_result\""
        );
    }
}
