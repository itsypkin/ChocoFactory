//! Shared HTTP error mapping (P1-9, design §6). One `ApiError` enum for
//! every handler rather than a per-handler status-code match, with `From`
//! impls translating the engine/db error types the handlers actually see.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::engine::{
    CancelTaskError, CreateTaskError, EngineError, ResolveError, SendMessageError,
    SendMessageOrResumeError,
};
use crate::session::SessionError;

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Conflict(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        ApiError::Internal(err.to_string())
    }
}

impl From<CreateTaskError> for ApiError {
    fn from(err: CreateTaskError) -> Self {
        match &err {
            CreateTaskError::Resolve(ResolveError::InvalidName(_)) => {
                ApiError::BadRequest(err.to_string())
            }
            CreateTaskError::Resolve(ResolveError::NotFound(_)) => {
                ApiError::NotFound(err.to_string())
            }
            CreateTaskError::NoSuchProject(_) | CreateTaskError::NoSuchParentTask(_) => {
                ApiError::NotFound(err.to_string())
            }
            CreateTaskError::Start(EngineError::MissingAgentTurnInput(_)) => {
                ApiError::BadRequest(err.to_string())
            }
            // The task row is written before `start_task` runs, so a cancel
            // can land in that window and `start_task`'s guard will refuse
            // to start it (#69). Someone cancelled the task out from under
            // the create — a conflict, not a server fault.
            CreateTaskError::Start(EngineError::TaskCancelled(_)) => {
                ApiError::Conflict(err.to_string())
            }
            _ => ApiError::Internal(err.to_string()),
        }
    }
}

impl From<SendMessageOrResumeError> for ApiError {
    fn from(err: SendMessageOrResumeError) -> Self {
        match &err {
            SendMessageOrResumeError::NoSuchTask | SendMessageOrResumeError::NoWorkflowState => {
                ApiError::NotFound(err.to_string())
            }
            SendMessageOrResumeError::UnsupportedStageKind(_)
            // A cancelled task refusing further input is the same shape of
            // conflict as a terminal one (#69): the request was
            // well-formed, the task just isn't in a state that can take it.
            | SendMessageOrResumeError::TaskCancelled => ApiError::Conflict(err.to_string()),
            // A `human_gate`'s `resumed` relay lost a race with another
            // caller resuming the same task concurrently (P1-9 review):
            // `advance()`'s own per-task lock means `workflow_state` is
            // never corrupted, but the loser's `advance` call re-reads
            // the *already-transitioned* stage and finds "resumed" isn't
            // one of its outcomes (or that it's now terminal) — a benign
            // "someone already resumed this" conflict, not a server
            // fault. `StageMovedOn` joins this list as of #59: threading a
            // `human_gate`'s capture through means this path now calls
            // `advance_from_stage` with `expected_stage` set (so the
            // capture lands on the stage that actually produced it), and
            // that's the exact same race surfacing through a stronger,
            // more specific check than `UnknownOutcome` used to catch it
            // with.
            SendMessageOrResumeError::Advance(
                EngineError::UnknownOutcome { .. }
                | EngineError::TerminalStageHasNoTransitions(_)
                | EngineError::StageMovedOn { .. }
                // The same benign race as the rest of this arm, arrived at
                // from the other direction (#69): a resume passed
                // `send_message_or_resume`'s status check and a cancel
                // landed before `advance_from_stage`'s guard ran under the
                // lock. "The task was cancelled while you were resuming it"
                // is a conflict, not a server fault — and mapping it to 500
                // here would contradict the plain `TaskCancelled` → 409
                // above.
                | EngineError::TaskCancelled(_),
            ) => ApiError::Conflict(err.to_string()),
            // Same conflict reached through the `agent_turn` branch, where
            // `send_message` re-checks the status under the per-task lock.
            SendMessageOrResumeError::SendMessage(SendMessageError::TaskCancelled) => {
                ApiError::Conflict(err.to_string())
            }
            _ => ApiError::Internal(err.to_string()),
        }
    }
}

impl From<CancelTaskError> for ApiError {
    fn from(err: CancelTaskError) -> Self {
        match &err {
            CancelTaskError::NoSuchTask | CancelTaskError::NoWorkflowState => {
                ApiError::NotFound(err.to_string())
            }
            // Already `cancelled` or already `closed`. A conflict rather
            // than a silent `202`: answering "accepted" to a cancel of work
            // that already finished would claim the daemon stopped
            // something it didn't.
            CancelTaskError::NotCancellable(_) => ApiError::Conflict(err.to_string()),
            // A session for this task is mid-spawn, so there was no process
            // to kill *yet* — retrying once it settles will find one. A
            // conflict, not a 500: nothing is broken, the caller just
            // arrived in the one-call-wide window where the answer would
            // have been a lie.
            //
            // `cancel_task` no longer propagates this (it logs instead, so
            // a failed kill can't strand an already-cancelled task), so
            // this arm is unreachable today and kept only so the mapping
            // stays right if that ever changes.
            CancelTaskError::Session(SessionError::AlreadyStarting) => {
                ApiError::Conflict(err.to_string())
            }
            _ => ApiError::Internal(err.to_string()),
        }
    }
}
