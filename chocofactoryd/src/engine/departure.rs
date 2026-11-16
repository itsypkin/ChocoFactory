//! Committing a stage departure: the one place that records "the task left
//! this stage with this outcome" and moves the task, as a single transaction.
//!
//! Three paths leave a stage for good — an ordinary outcome
//! (`advance_from_stage`), a parallel group whose branches are all done
//! (`apply_branch_end`) and a watcher timeout's retry back to the watcher
//! (`rewatch_locked`). Each computes its destination its own way and then
//! hands the prepared state update here, so a result and the move it
//! describes can never be written one without the other, and the new stage is
//! only started after both are durable.

use super::*;
use chocofactory_core::context::{self as results, StageResultPayload};
use chocofactory_core::models::WorkflowState;

/// What the task is leaving: the stage, the outcome applied and the capture
/// the transition was given, as they were *before* the transition merged the
/// capture into the payload.
pub(super) struct Departure<'a> {
    pub(super) source_stage: &'a str,
    pub(super) source_kind: &'a str,
    pub(super) outcome: &'a str,
    pub(super) capture: Option<Value>,
    /// The result pointer read from the state being left.
    pub(super) previous_result_id: Option<String>,
}

/// The newest result's ID in `state`, or `None` for a task from before
/// results were recorded. A pointer that is present but not an ID is an
/// error: treating it as a fresh start would silently cut the chain.
pub(super) fn previous_result_id(state: &WorkflowState) -> Result<Option<String>, EngineError> {
    results::current_result_id(&state.payload)
        .map_err(|err| EngineError::MalformedResultPointer(err.0))
}

/// The two writes of a departure on the caller's transaction connection:
/// the stage result, then the state update that points at it. Neither is
/// durable until the caller commits; dropping the transaction first loses
/// both. `Err(NoWorkflowState)` means the row to move was not there.
pub(super) async fn write_departure(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    departure: Departure<'_>,
    mut update: workflow_state::WorkflowStateUpdate,
) -> Result<WorkflowState, EngineError> {
    let payload = StageResultPayload::new(
        departure.source_stage,
        departure.source_kind,
        departure.outcome,
        &update.current_stage,
        departure.capture,
        departure.previous_result_id,
    );
    let event =
        events::append_for_task_on(conn, task_id, EventType::StageResult, payload.to_value())
            .await?;
    results::set_current_result_id(&mut update.payload, &event.id);
    workflow_state::update_in(conn, task_id, update)
        .await?
        .ok_or(EngineError::NoWorkflowState)
}

impl WorkflowEngine {
    /// Records the departure and writes the prepared state update in one
    /// transaction ([`write_departure`]). Returns the committed row, which
    /// the next stage renders against. Any failure — the insert, the update,
    /// a vanished row, the commit — drops the transaction, so neither half
    /// lands, and the caller must not start the destination.
    ///
    /// Readers waiting on the timeline are woken only after the commit.
    pub(super) async fn commit_stage_departure(
        &self,
        task_id: &str,
        departure: Departure<'_>,
        update: workflow_state::WorkflowStateUpdate,
    ) -> Result<WorkflowState, EngineError> {
        let mut tx = self.pool.begin().await?;
        let updated = write_departure(&mut tx, task_id, departure, update).await?;
        tx.commit().await?;
        self.events_notify.notify_waiters();
        Ok(updated)
    }
}
