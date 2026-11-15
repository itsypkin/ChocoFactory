//! The read-only lookup behind `GET /tasks/{id}/context`: one requested value
//! about a task — its own facts, or an applied stage result — never a
//! transcript or a list.
//!
//! Applied stage results are read by following `previous_result_id` links
//! backwards from a fixed anchor (the task's current result, or one the
//! caller supplied). Nothing here orders by timestamp, by ID, or by the
//! diagnostic events around the results.
//!
//! Everything for one request is read inside a single SELECT-only
//! transaction on one connection, so the pointer, the captures and the chain
//! all come from the same database snapshot. The transaction ends when the
//! lookup returns or its future is dropped: no lock is kept, no row written,
//! no work left running.

use std::collections::HashSet;
use std::future::Future;

use chocofactory_core::context::{
    AppliedStageResult, ContextResponse, TaskContext, UnavailableReason, current_result_id,
};
use chocofactory_core::models::{Task, WorkflowState};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::db::{events, tasks, workflow_state};

/// What a caller asks for. Built by the HTTP handler after it has validated
/// the query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRequest {
    /// The task's own title, initial input and closing issues.
    Task,
    /// The latest applied result of `stage` (optionally with exactly
    /// `outcome`) at or before the anchor.
    StageByName {
        stage: String,
        outcome: Option<String>,
        anchor_id: Option<String>,
    },
    /// One exact result, optionally only if it is at or before the anchor.
    StageById {
        result_id: String,
        anchor_id: Option<String>,
    },
}

/// The answer, and how the HTTP layer should report it.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// No such task: the existing 404 error response.
    NoSuchTask,
    /// A normal answer (status 200), including most "unavailable" ones.
    Found(ContextResponse),
    /// The caller supplied a result or anchor ID that cannot be read as a
    /// stage result of this task (status 404). Missing, pruned, another
    /// task's and wrong-type IDs all look the same.
    UnknownId(ContextResponse),
}

/// Looks up `request` for `task_id`. Storage failures are errors; they are
/// never turned into an "unavailable" answer.
pub async fn lookup(
    pool: &SqlitePool,
    task_id: &str,
    request: &ContextRequest,
) -> Result<Lookup, sqlx::Error> {
    lookup_with_pause(pool, task_id, request, || async {}).await
}

/// [`lookup`] that awaits `pause` once the anchor has been read and before
/// the rest of the reads, so a test can hold the read transaction open at
/// that point.
pub(crate) async fn lookup_with_pause<F, Fut>(
    pool: &SqlitePool,
    task_id: &str,
    request: &ContextRequest,
    pause: F,
) -> Result<Lookup, sqlx::Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut tx = pool.begin().await?;
    let answer = lookup_in(&mut tx, task_id, request, pause).await?;
    // Read-only: nothing to commit. Ending it here rather than at drop keeps
    // the connection's lock from outliving the answer.
    tx.rollback().await?;
    Ok(answer)
}

enum Read {
    /// Not a stage result of this task (absent, pruned, another task's, a
    /// session event or another event type).
    Missing,
    Malformed,
    Found(AppliedStageResult),
}

async fn read_result(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    id: &str,
) -> Result<Read, sqlx::Error> {
    Ok(match events::get_stage_result_in(conn, task_id, id).await? {
        None => Read::Missing,
        Some(event) => match AppliedStageResult::from_event(&event) {
            Ok(result) => Read::Found(result),
            Err(_) => Read::Malformed,
        },
    })
}

async fn lookup_in<F, Fut>(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    request: &ContextRequest,
    pause: F,
) -> Result<Lookup, sqlx::Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(task) = tasks::get_in(conn, task_id).await? else {
        return Ok(Lookup::NoSuchTask);
    };
    match request {
        ContextRequest::Task => {
            let state = workflow_state::get_in(conn, task_id).await?;
            Ok(Lookup::Found(task_response(&task, state.as_ref())))
        }
        ContextRequest::StageByName {
            stage,
            outcome,
            anchor_id,
        } => {
            let anchor = match resolve_anchor(conn, task_id, anchor_id.as_deref(), stage, outcome)
                .await?
            {
                Anchor::Start(anchor) => anchor,
                Anchor::Answer(answer) => return Ok(answer),
            };
            pause().await;
            let walked = walk(conn, task_id, anchor.clone(), |r| {
                r.source_stage == *stage && outcome.as_ref().is_none_or(|o| r.outcome == *o)
            })
            .await?;
            Ok(Lookup::Found(match walked {
                Walk::Found(result) => ContextResponse::StageResult {
                    task_id: task_id.to_string(),
                    anchor_id: Some(anchor.result_id),
                    result,
                },
                Walk::EndOfChain => unavailable(
                    task_id,
                    Some(anchor.result_id),
                    UnavailableReason::NotFoundInRecordedHistory,
                ),
                Walk::Gap => unavailable(
                    task_id,
                    Some(anchor.result_id),
                    UnavailableReason::HistoryGap,
                ),
                Walk::Malformed => unavailable(
                    task_id,
                    Some(anchor.result_id),
                    UnavailableReason::MalformedHistory,
                ),
            }))
        }
        ContextRequest::StageById {
            result_id,
            anchor_id,
        } => {
            // A supplied anchor is validated before the result it bounds.
            let anchor = match anchor_id {
                None => None,
                Some(id) => match read_result(conn, task_id, id).await? {
                    Read::Missing => {
                        return Ok(Lookup::UnknownId(unavailable(
                            task_id,
                            Some(id.clone()),
                            UnavailableReason::MissingAnchor,
                        )));
                    }
                    Read::Malformed => {
                        return Ok(Lookup::Found(unavailable(
                            task_id,
                            Some(id.clone()),
                            UnavailableReason::MalformedHistory,
                        )));
                    }
                    Read::Found(anchor) => Some(anchor),
                },
            };
            let exact = match read_result(conn, task_id, result_id).await? {
                Read::Missing => {
                    return Ok(Lookup::UnknownId(unavailable(
                        task_id,
                        anchor_id.clone(),
                        UnavailableReason::MissingResult,
                    )));
                }
                Read::Malformed => {
                    return Ok(Lookup::Found(unavailable(
                        task_id,
                        anchor_id.clone(),
                        UnavailableReason::MalformedHistory,
                    )));
                }
                Read::Found(result) => result,
            };
            let Some(anchor) = anchor else {
                return Ok(Lookup::Found(ContextResponse::StageResult {
                    task_id: task_id.to_string(),
                    anchor_id: None,
                    result: exact,
                }));
            };
            pause().await;
            let anchor_id = Some(anchor.result_id.clone());
            let walked = walk(conn, task_id, anchor, |r| r.result_id == *result_id).await?;
            Ok(Lookup::Found(match walked {
                Walk::Found(result) => ContextResponse::StageResult {
                    task_id: task_id.to_string(),
                    anchor_id,
                    result,
                },
                Walk::EndOfChain => {
                    unavailable(task_id, anchor_id, UnavailableReason::NotAtOrBeforeAnchor)
                }
                Walk::Gap => unavailable(task_id, anchor_id, UnavailableReason::HistoryGap),
                Walk::Malformed => {
                    unavailable(task_id, anchor_id, UnavailableReason::MalformedHistory)
                }
            }))
        }
    }
}

fn unavailable(task_id: &str, anchor_id: Option<String>, reason: UnavailableReason) -> ContextResponse {
    ContextResponse::unavailable(task_id, anchor_id, reason)
}

enum Anchor {
    /// Where to start walking.
    Start(AppliedStageResult),
    /// The request is already answered.
    Answer(Lookup),
}

/// Fixes the anchor of a stage-by-name request: the supplied one (verified as
/// a result of this task), else the task's current result, read once. A task
/// from before results were recorded (no pointer) is answered from its latest
/// stored capture instead.
async fn resolve_anchor(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    supplied: Option<&str>,
    stage: &str,
    outcome: &Option<String>,
) -> Result<Anchor, sqlx::Error> {
    if let Some(id) = supplied {
        return Ok(match read_result(conn, task_id, id).await? {
            Read::Found(anchor) => Anchor::Start(anchor),
            Read::Missing => Anchor::Answer(Lookup::UnknownId(unavailable(
                task_id,
                Some(id.to_string()),
                UnavailableReason::MissingAnchor,
            ))),
            Read::Malformed => Anchor::Answer(Lookup::Found(unavailable(
                task_id,
                Some(id.to_string()),
                UnavailableReason::MalformedHistory,
            ))),
        });
    }
    let Some(state) = workflow_state::get_in(conn, task_id).await? else {
        return Ok(Anchor::Answer(Lookup::Found(unavailable(
            task_id,
            None,
            UnavailableReason::MissingWorkflowState,
        ))));
    };
    let pointer = match current_result_id(&state.payload) {
        Ok(pointer) => pointer,
        Err(_) => {
            return Ok(Anchor::Answer(Lookup::Found(unavailable(
                task_id,
                None,
                UnavailableReason::MalformedHistory,
            ))));
        }
    };
    let Some(id) = pointer else {
        return Ok(Anchor::Answer(Lookup::Found(legacy_snapshot(
            task_id, &state, stage, outcome,
        ))));
    };
    Ok(match read_result(conn, task_id, &id).await? {
        Read::Found(anchor) => Anchor::Start(anchor),
        Read::Missing => Anchor::Answer(Lookup::Found(unavailable(
            task_id,
            Some(id),
            UnavailableReason::MissingAnchor,
        ))),
        Read::Malformed => Anchor::Answer(Lookup::Found(unavailable(
            task_id,
            Some(id),
            UnavailableReason::MalformedHistory,
        ))),
    })
}

/// A task from before results were recorded: its latest capture of `stage`,
/// honestly marked as a snapshot. It cannot say which outcome was applied, so
/// an outcome filter is never satisfied from it — not even by an `outcome` key
/// inside the captured value.
fn legacy_snapshot(
    task_id: &str,
    state: &WorkflowState,
    stage: &str,
    outcome: &Option<String>,
) -> ContextResponse {
    let capture = state
        .payload
        .get("stages")
        .and_then(Value::as_object)
        .and_then(|stages| stages.get(stage));
    match capture {
        None => unavailable(task_id, None, UnavailableReason::MissingSnapshot),
        Some(_) if outcome.is_some() => unavailable(
            task_id,
            None,
            UnavailableReason::UnverifiableSnapshotOutcome,
        ),
        Some(capture) => ContextResponse::Snapshot {
            task_id: task_id.to_string(),
            anchor_id: None,
            stage: stage.to_string(),
            capture_present: true,
            capture: capture.clone(),
        },
    }
}

enum Walk {
    Found(AppliedStageResult),
    /// The oldest recorded result was reached without a match.
    EndOfChain,
    /// A link needed to go further cannot be read.
    Gap,
    /// A result or link has a shape this version does not write, or the
    /// chain loops.
    Malformed,
}

/// Follows `previous_result_id` links from `start` (inclusive), returning the
/// first result `is_match` accepts. Reads only as far as the answer needs, and
/// stops at the first repeated ID rather than looping on a corrupt chain.
async fn walk(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    start: AppliedStageResult,
    is_match: impl Fn(&AppliedStageResult) -> bool,
) -> Result<Walk, sqlx::Error> {
    let mut seen = HashSet::new();
    seen.insert(start.result_id.clone());
    let mut current = start;
    loop {
        if is_match(&current) {
            return Ok(Walk::Found(current));
        }
        let Some(previous) = current.previous_result_id.clone() else {
            return Ok(Walk::EndOfChain);
        };
        if !seen.insert(previous.clone()) {
            return Ok(Walk::Malformed);
        }
        current = match read_result(conn, task_id, &previous).await? {
            Read::Found(result) => result,
            Read::Missing => return Ok(Walk::Gap),
            Read::Malformed => return Ok(Walk::Malformed),
        };
    }
}

/// The task's own facts. The input comes only from the seeded
/// `payload.task.input`; a missing, null or non-string value is unavailable
/// rather than rebuilt from messages. The title prefers the seeded value.
fn task_response(task: &Task, state: Option<&WorkflowState>) -> ContextResponse {
    let seeded = state.and_then(|state| state.payload.get("task"));
    let input = seeded
        .and_then(|task| task.get("input"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let title = seeded
        .and_then(|task| task.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map_or_else(|| task.title.clone(), str::to_string);
    ContextResponse::Task {
        task_id: task.id.clone(),
        anchor_id: None,
        task: TaskContext {
            title,
            input_available: input.is_some(),
            input,
            closes: task.closes.clone(),
        },
    }
}
