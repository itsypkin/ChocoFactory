//! The shared shapes of committed stage results and of the task-context
//! lookup (`GET /tasks/{id}/context`).
//!
//! Every time a task actually leaves a stage, the engine records one
//! [`EventType::StageResult`] event in the same transaction as the workflow
//! state it moves. The event's own ID is the result's stable ID, and
//! [`StageResultPayload::previous_result_id`] links it to the departure
//! before it, so the applied history is a chain that never depends on
//! timestamps or ID order. The newest link is kept in the workflow payload
//! under [`CURRENT_RESULT_ID_KEY`].
//!
//! The writer (engine) and the reader (lookup service) both go through the
//! types and helpers here, so the payload shape and the pointer format cannot
//! drift between them.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::models::{Event, EventType};

/// Where the engine keeps the ID of the newest stage result inside
/// `workflow_state.payload`. Engine-owned: never a workflow capture key.
pub const CURRENT_RESULT_ID_KEY: &str = "current_result_id";

/// A stored stage result, or the pointer to one, has a shape this version
/// does not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedHistory(pub String);

impl fmt::Display for MalformedHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed stage-result history: {}", self.0)
    }
}

impl std::error::Error for MalformedHistory {}

/// Reads the engine's pointer to the newest stage result. A missing key or a
/// JSON null is the boundary from before results were recorded (`Ok(None)`);
/// any other value that is not a non-empty string is malformed, never a fresh
/// start.
pub fn current_result_id(payload: &Value) -> Result<Option<String>, MalformedHistory> {
    match payload.get(CURRENT_RESULT_ID_KEY) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id.clone())),
        Some(other) => Err(MalformedHistory(format!(
            "{CURRENT_RESULT_ID_KEY} is {other}, not a result ID"
        ))),
    }
}

/// Points `payload` at `result_id`. A payload that is not an object is
/// replaced by one, as every other engine-owned payload write does.
pub fn set_current_result_id(payload: &mut Value, result_id: &str) {
    if !payload.is_object() {
        *payload = json!({});
    }
    payload
        .as_object_mut()
        .expect("payload was just ensured to be an object")
        .insert(CURRENT_RESULT_ID_KEY.to_string(), json!(result_id));
}

/// The transition facts stored in a stage-result event's payload. The task,
/// the result ID and the timestamp come from the event's own columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageResultPayload {
    pub source_stage: String,
    /// The source stage's kind as workflow YAML spells it (`agent_turn`,
    /// `shell`, `poll`, `human_gate` or `parallel`).
    pub source_kind: String,
    /// The outcome the engine actually applied.
    pub outcome: String,
    /// Where the task actually went, after any loop-guard redirect.
    pub destination_stage: String,
    /// `true` with a null `capture` is a captured JSON null; `false` means
    /// the departure carried no captured value.
    pub capture_present: bool,
    pub capture: Value,
    pub previous_result_id: Option<String>,
}

impl StageResultPayload {
    pub fn new(
        source_stage: &str,
        source_kind: &str,
        outcome: &str,
        destination_stage: &str,
        capture: Option<Value>,
        previous_result_id: Option<String>,
    ) -> Self {
        Self {
            source_stage: source_stage.to_string(),
            source_kind: source_kind.to_string(),
            outcome: outcome.to_string(),
            destination_stage: destination_stage.to_string(),
            capture_present: capture.is_some(),
            capture: capture.unwrap_or(Value::Null),
            previous_result_id,
        }
    }

    /// The JSON stored as the event's payload.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("a stage result payload always serializes")
    }
}

/// An applied stage result as the lookup returns it: the stored facts plus
/// the task, the stable result ID and the event's timestamp (a record of when
/// it was written, not an ordering authority).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedStageResult {
    pub task_id: String,
    pub result_id: String,
    pub source_stage: String,
    pub source_kind: String,
    pub outcome: String,
    pub destination_stage: String,
    pub capture_present: bool,
    pub capture: Value,
    pub previous_result_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl AppliedStageResult {
    /// Decodes a stage-result event. Anything this version would not have
    /// written is [`MalformedHistory`], never a partial result.
    pub fn from_event(event: &Event) -> Result<Self, MalformedHistory> {
        let bad = |what: &str| MalformedHistory(format!("result {}: {what}", event.id));
        if event.event_type != EventType::StageResult {
            return Err(bad("not a stage_result event"));
        }
        // A missing link key would read as the end of the chain.
        if event.payload.get("previous_result_id").is_none() {
            return Err(bad("previous_result_id is missing"));
        }
        let stored: StageResultPayload = serde_json::from_value(event.payload.clone())
            .map_err(|err| bad(&format!("unreadable payload: {err}")))?;
        if stored.source_stage.is_empty()
            || stored.source_kind.is_empty()
            || stored.outcome.is_empty()
            || stored.destination_stage.is_empty()
        {
            return Err(bad("a stage, kind or outcome is empty"));
        }
        if stored.previous_result_id.as_deref() == Some("") {
            return Err(bad("previous_result_id is empty"));
        }
        if !stored.capture_present && !stored.capture.is_null() {
            return Err(bad("capture is set but capture_present is false"));
        }
        Ok(Self {
            task_id: event.task_id.clone(),
            result_id: event.id.clone(),
            source_stage: stored.source_stage,
            source_kind: stored.source_kind,
            outcome: stored.outcome,
            destination_stage: stored.destination_stage,
            capture_present: stored.capture_present,
            capture: stored.capture,
            previous_result_id: stored.previous_result_id,
            created_at: event.created_at,
        })
    }
}

/// The task's own facts as the lookup returns them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskContext {
    pub title: String,
    pub input_available: bool,
    pub input: Option<String>,
    pub closes: Vec<u64>,
}

/// Why a requested value could not be returned. None of these means a
/// previous result never existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The task has no workflow state row.
    MissingWorkflowState,
    /// The requested result ID cannot be read as a result of this task.
    MissingResult,
    /// The anchor (supplied, or the current pointer) cannot be read as a
    /// result of this task.
    MissingAnchor,
    /// A link needed to answer is not readable (pruned, or not this task's).
    HistoryGap,
    /// The recorded chain ends without a match.
    NotFoundInRecordedHistory,
    /// The chain from the anchor was fully read and does not reach the result.
    NotAtOrBeforeAnchor,
    /// A task from before results were recorded has no capture for the stage.
    MissingSnapshot,
    /// Only a legacy latest-value capture exists, and it cannot confirm which
    /// outcome was applied.
    UnverifiableSnapshotOutcome,
    /// A pointer, result or link has a shape this version does not write, or
    /// the chain loops.
    MalformedHistory,
}

/// The body of `GET /tasks/{id}/context`: exactly one requested value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provenance", rename_all = "snake_case")]
pub enum ContextResponse {
    Task {
        task_id: String,
        anchor_id: Option<String>,
        task: TaskContext,
    },
    StageResult {
        task_id: String,
        anchor_id: Option<String>,
        result: AppliedStageResult,
    },
    /// The latest capture of a task that predates stage results: a stored
    /// value, not an applied result, so it carries no result metadata.
    Snapshot {
        task_id: String,
        anchor_id: Option<String>,
        stage: String,
        capture_present: bool,
        capture: Value,
    },
    Unavailable {
        task_id: String,
        anchor_id: Option<String>,
        reason: UnavailableReason,
    },
}

impl ContextResponse {
    pub fn unavailable(
        task_id: &str,
        anchor_id: Option<String>,
        reason: UnavailableReason,
    ) -> Self {
        Self::Unavailable {
            task_id: task_id.to_string(),
            anchor_id,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(payload: Value) -> Event {
        Event {
            id: "r2".into(),
            task_id: "t".into(),
            session_id: None,
            event_type: EventType::StageResult,
            payload,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn payload_round_trips_to_the_public_result() {
        let stored = StageResultPayload::new(
            "review",
            "human_gate",
            "rejected",
            "coding",
            Some(json!("fix A and B")),
            Some("r1".into()),
        );
        let ev = event(stored.to_value());
        let result = AppliedStageResult::from_event(&ev).unwrap();
        assert_eq!(result.result_id, "r2");
        assert_eq!(result.task_id, "t");
        assert_eq!(result.source_stage, "review");
        assert_eq!(result.outcome, "rejected");
        assert_eq!(result.destination_stage, "coding");
        assert!(result.capture_present);
        assert_eq!(result.capture, json!("fix A and B"));
        assert_eq!(result.previous_result_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_null_capture_and_an_absent_capture_stay_distinct() {
        let null = StageResultPayload::new("a", "shell", "ok", "b", Some(Value::Null), None);
        let absent = StageResultPayload::new("a", "shell", "ok", "b", None, None);
        assert!(null.capture_present && null.capture.is_null());
        assert!(!absent.capture_present && absent.capture.is_null());
        assert_ne!(null.to_value(), absent.to_value());
    }

    #[test]
    fn malformed_payloads_are_refused() {
        let good = StageResultPayload::new("a", "shell", "ok", "b", None, None).to_value();
        assert!(AppliedStageResult::from_event(&event(good.clone())).is_ok());
        for broken in [
            json!(null),
            json!([]),
            json!("text"),
            json!({"source_stage": "a"}),
            {
                let mut v = good.clone();
                v.as_object_mut().unwrap().remove("previous_result_id");
                v
            },
            {
                let mut v = good.clone();
                v["previous_result_id"] = json!("");
                v
            },
            {
                let mut v = good.clone();
                v["source_stage"] = json!("");
                v
            },
            {
                let mut v = good.clone();
                v["capture_present"] = json!("yes");
                v
            },
            {
                let mut v = good.clone();
                v["capture"] = json!("stray");
                v
            },
        ] {
            assert!(
                AppliedStageResult::from_event(&event(broken.clone())).is_err(),
                "{broken} should be malformed"
            );
        }
    }

    #[test]
    fn current_result_id_distinguishes_absent_from_malformed() {
        assert_eq!(current_result_id(&json!({})), Ok(None));
        assert_eq!(current_result_id(&json!(null)), Ok(None));
        assert_eq!(current_result_id(&json!([1])), Ok(None));
        assert_eq!(
            current_result_id(&json!({"current_result_id": null})),
            Ok(None)
        );
        assert_eq!(
            current_result_id(&json!({"current_result_id": "r1"})),
            Ok(Some("r1".to_string()))
        );
        for bad in [json!(""), json!(7), json!([]), json!({}), json!(true)] {
            assert!(current_result_id(&json!({"current_result_id": bad})).is_err());
        }
    }

    #[test]
    fn set_current_result_id_replaces_a_non_object_payload() {
        let mut payload = json!([1, 2]);
        set_current_result_id(&mut payload, "r9");
        assert_eq!(payload, json!({"current_result_id": "r9"}));
    }

    #[test]
    fn the_response_is_tagged_by_provenance() {
        let unavailable = ContextResponse::unavailable("t", None, UnavailableReason::HistoryGap);
        assert_eq!(
            serde_json::to_value(&unavailable).unwrap(),
            json!({"provenance": "unavailable", "task_id": "t", "anchor_id": null,
                   "reason": "history_gap"})
        );
    }
}
