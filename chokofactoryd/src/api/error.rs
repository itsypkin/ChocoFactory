//! Shared HTTP error mapping (P1-9, design §6). One `ApiError` enum for
//! every handler rather than a per-handler status-code match, with `From`
//! impls translating the engine/db error types the handlers actually see.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::engine::{CreateTaskError, EngineError, ResolveError, SendMessageOrResumeError};

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
            SendMessageOrResumeError::UnsupportedStageKind(_) => {
                ApiError::Conflict(err.to_string())
            }
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
                | EngineError::StageMovedOn { .. },
            ) => ApiError::Conflict(err.to_string()),
            _ => ApiError::Internal(err.to_string()),
        }
    }
}
