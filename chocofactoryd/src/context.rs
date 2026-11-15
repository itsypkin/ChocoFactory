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
    Found(Box<AppliedStageResult>),
}

async fn read_result(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    id: &str,
) -> Result<Read, sqlx::Error> {
    Ok(
        match events::get_stage_result_in(conn, task_id, id).await? {
            None => Read::Missing,
            Some(event) => match AppliedStageResult::from_event(&event) {
                Ok(result) => Read::Found(Box::new(result)),
                Err(_) => Read::Malformed,
            },
        },
    )
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
            let anchor =
                match resolve_anchor(conn, task_id, anchor_id.as_deref(), stage, outcome).await? {
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
                    result: *result,
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
                    Read::Found(anchor) => Some(*anchor),
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
                Read::Found(result) => *result,
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
                    result: *result,
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

fn unavailable(
    task_id: &str,
    anchor_id: Option<String>,
    reason: UnavailableReason,
) -> ContextResponse {
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
            Read::Found(anchor) => Anchor::Start(*anchor),
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
        Read::Found(anchor) => Anchor::Start(*anchor),
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
    // The outcome filter is decided first: a stored capture, present or
    // not, can never say which outcome was applied.
    if outcome.is_some() {
        return unavailable(
            task_id,
            None,
            UnavailableReason::UnverifiableSnapshotOutcome,
        );
    }
    let capture = state
        .payload
        .get("stages")
        .and_then(Value::as_object)
        .and_then(|stages| stages.get(stage));
    match capture {
        None => unavailable(task_id, None, UnavailableReason::MissingSnapshot),
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
    Found(Box<AppliedStageResult>),
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
            return Ok(Walk::Found(Box::new(current)));
        }
        let Some(previous) = current.previous_result_id.clone() else {
            return Ok(Walk::EndOfChain);
        };
        if !seen.insert(previous.clone()) {
            return Ok(Walk::Malformed);
        }
        current = match read_result(conn, task_id, &previous).await? {
            Read::Found(result) => *result,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect, connect_in_memory, events as db_events, projects, sessions};
    use chocofactory_core::context::StageResultPayload;
    use chocofactory_core::models::EventType;
    use chrono::{Duration, Utc};
    use serde_json::json;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    async fn new_task_in(pool: &SqlitePool, title: &str, closes: &[u64]) -> String {
        let project_id = projects::create(pool, &format!("p-{title}"), None)
            .await
            .unwrap()
            .id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "wf",
                title,
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
                base_ref: None,
                base_commit: None,
                closes,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn new_task(pool: &SqlitePool) -> String {
        new_task_in(pool, "Fix the thing", &[7, 12]).await
    }

    async fn put_state(pool: &SqlitePool, task: &str, payload: Value) {
        workflow_state::create(pool, task, "review", "human_gate", payload)
            .await
            .unwrap();
    }

    /// A stage result with a chosen ID, written directly. `age_hours` is how
    /// old its timestamp claims to be, so tests can contradict the chain.
    #[allow(clippy::too_many_arguments)]
    async fn put_result(
        pool: &SqlitePool,
        task: &str,
        id: &str,
        previous: Option<&str>,
        stage: &str,
        outcome: &str,
        capture: Option<Value>,
        age_hours: i64,
    ) {
        let payload = StageResultPayload::new(
            stage,
            "human_gate",
            outcome,
            "next",
            capture,
            previous.map(str::to_string),
        )
        .to_value();
        put_raw_event(pool, task, id, "stage_result", None, payload, age_hours).await;
    }

    async fn put_raw_event(
        pool: &SqlitePool,
        task: &str,
        id: &str,
        event_type: &str,
        session: Option<&str>,
        payload: Value,
        age_hours: i64,
    ) {
        sqlx::query(
            "INSERT INTO events (id, task_id, session_id, event_type, payload, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(task)
        .bind(session)
        .bind(event_type)
        .bind(sqlx::types::Json(payload))
        .bind(Utc::now() - Duration::hours(age_hours))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn point_at(pool: &SqlitePool, task: &str, id: Value) {
        let state = workflow_state::get(pool, task).await.unwrap().unwrap();
        let mut payload = state.payload;
        if !payload.is_object() {
            payload = json!({});
        }
        payload["current_result_id"] = id;
        workflow_state::update(
            pool,
            task,
            workflow_state::WorkflowStateUpdate {
                current_stage: state.current_stage,
                stage_kind: "human_gate".into(),
                loop_counters: state.loop_counters,
                payload,
                enters_stage: false,
            },
        )
        .await
        .unwrap();
    }

    fn by_name(stage: &str, outcome: Option<&str>, anchor: Option<&str>) -> ContextRequest {
        ContextRequest::StageByName {
            stage: stage.into(),
            outcome: outcome.map(Into::into),
            anchor_id: anchor.map(Into::into),
        }
    }

    fn by_id(id: &str, anchor: Option<&str>) -> ContextRequest {
        ContextRequest::StageById {
            result_id: id.into(),
            anchor_id: anchor.map(Into::into),
        }
    }

    async fn ask(pool: &SqlitePool, task: &str, req: &ContextRequest) -> Lookup {
        tokio::time::timeout(std::time::Duration::from_secs(10), lookup(pool, task, req))
            .await
            .expect("the lookup must finish")
            .unwrap()
    }

    fn unavailable_with(answer: &Lookup, expected_reason: UnavailableReason) -> Option<String> {
        let response = match answer {
            Lookup::Found(r) | Lookup::UnknownId(r) => r,
            Lookup::NoSuchTask => panic!("no such task"),
        };
        match response {
            ContextResponse::Unavailable {
                reason, anchor_id, ..
            } => {
                assert_eq!(*reason, expected_reason, "{response:?}");
                anchor_id.clone()
            }
            other => panic!("expected unavailable/{expected_reason:?}, got {other:?}"),
        }
    }

    fn result_of(answer: &Lookup) -> (AppliedStageResult, Option<String>) {
        match answer {
            Lookup::Found(ContextResponse::StageResult {
                result, anchor_id, ..
            }) => (result.clone(), anchor_id.clone()),
            other => panic!("expected an applied result, got {other:?}"),
        }
    }

    /// r1 (stage s, x) <- r2 (stage t) <- r3 (stage s, y), pointer at r3,
    /// with timestamps in the opposite order of the chain.
    async fn seeded_chain(pool: &SqlitePool) -> String {
        let task = new_task(pool).await;
        put_state(
            pool,
            &task,
            json!({"task": {"input": "do it", "title": "Seeded title"}}),
        )
        .await;
        put_result(pool, &task, "r1", None, "s", "x", Some(json!("first")), 1).await;
        put_result(
            pool,
            &task,
            "r2",
            Some("r1"),
            "t",
            "x",
            Some(json!({"k": 1})),
            2,
        )
        .await;
        put_result(pool, &task, "r3", Some("r2"), "s", "y", None, 3).await;
        point_at(pool, &task, json!("r3")).await;
        task
    }

    #[tokio::test]
    async fn the_chain_not_the_timestamps_picks_the_result() {
        let pool = connect_in_memory().await.unwrap();
        let task = seeded_chain(&pool).await;

        // Latest `s` at the current anchor: r3, with its absent capture still
        // an applied result.
        let (r, anchor) = result_of(&ask(&pool, &task, &by_name("s", None, None)).await);
        assert_eq!(r.result_id, "r3");
        assert_eq!(anchor.as_deref(), Some("r3"));
        assert!(!r.capture_present);
        assert_eq!(r.capture, Value::Null);
        // An outcome narrows it.
        let (r, _) = result_of(&ask(&pool, &task, &by_name("s", Some("x"), None)).await);
        assert_eq!((r.result_id.as_str(), r.capture), ("r1", json!("first")));
        // The explicit anchor fixes the selection even after later commits.
        let (r, anchor) = result_of(&ask(&pool, &task, &by_name("s", None, Some("r2"))).await);
        assert_eq!(r.result_id, "r1");
        assert_eq!(anchor.as_deref(), Some("r2"));
        // Inclusive: the anchor itself can be the match.
        let (r, _) = result_of(&ask(&pool, &task, &by_name("t", None, Some("r2"))).await);
        assert_eq!(r.result_id, "r2");
        assert_eq!(r.capture, json!({"k": 1}));
        // Exact IDs, with and without an anchor.
        let (r, anchor) = result_of(&ask(&pool, &task, &by_id("r1", None)).await);
        assert_eq!((r.result_id.as_str(), anchor), ("r1", None));
        let (r, anchor) = result_of(&ask(&pool, &task, &by_id("r2", Some("r3"))).await);
        assert_eq!(
            (r.result_id.as_str(), anchor.as_deref()),
            ("r2", Some("r3"))
        );
        // A result newer than the anchor is refused, with no content.
        let answer = ask(&pool, &task, &by_id("r3", Some("r2"))).await;
        assert_eq!(
            unavailable_with(&answer, UnavailableReason::NotAtOrBeforeAnchor).as_deref(),
            Some("r2")
        );
        // A stage with no result: the recorded chain ends without a match.
        let answer = ask(&pool, &task, &by_name("nowhere", None, None)).await;
        unavailable_with(&answer, UnavailableReason::NotFoundInRecordedHistory);
        // A name and an outcome that never co-occur.
        let answer = ask(&pool, &task, &by_name("t", Some("y"), None)).await;
        unavailable_with(&answer, UnavailableReason::NotFoundInRecordedHistory);
    }

    #[tokio::test]
    async fn foreign_wrong_type_and_session_rows_are_all_just_unknown_ids() {
        let pool = connect_in_memory().await.unwrap();
        let mine = seeded_chain(&pool).await;
        let other = new_task_in(&pool, "Other", &[]).await;
        put_state(&pool, &other, json!({})).await;
        put_result(
            &pool,
            &other,
            "foreign",
            None,
            "s",
            "x",
            Some(json!("SECRET OF B")),
            1,
        )
        .await;
        put_raw_event(
            &pool,
            &mine,
            "chatter",
            "assistant_message",
            None,
            json!({"text": "hi"}),
            1,
        )
        .await;
        let session = sessions::create(
            &pool,
            sessions::NewSession {
                task_id: &mine,
                stage: "s",
                role: "coder",
                cli_adapter: "claude",
                model: "sonnet",
                cleanup: None,
            },
        )
        .await
        .unwrap()
        .id;
        let attributed =
            StageResultPayload::new("s", "agent_turn", "x", "t", None, None).to_value();
        put_raw_event(
            &pool,
            &mine,
            "attributed",
            "stage_result",
            Some(&session),
            attributed,
            1,
        )
        .await;

        for id in ["foreign", "chatter", "attributed", "does-not-exist"] {
            // An ID, as the result or as the anchor, is refused identically.
            let answer = ask(&pool, &mine, &by_id(id, None)).await;
            assert!(matches!(answer, Lookup::UnknownId(_)), "{id}: {answer:?}");
            unavailable_with(&answer, UnavailableReason::MissingResult);
            let answer = ask(&pool, &mine, &by_name("s", None, Some(id))).await;
            assert!(matches!(answer, Lookup::UnknownId(_)), "{id}: {answer:?}");
            assert_eq!(
                unavailable_with(&answer, UnavailableReason::MissingAnchor).as_deref(),
                Some(id)
            );
            let answer = ask(&pool, &mine, &by_id("r1", Some(id))).await;
            assert!(matches!(answer, Lookup::UnknownId(_)), "{id}: {answer:?}");
            unavailable_with(&answer, UnavailableReason::MissingAnchor);
            assert!(!format!("{answer:?}").contains("SECRET OF B"));
        }
        // The anchor is validated before the result it bounds.
        let answer = ask(&pool, &mine, &by_id("does-not-exist", Some("foreign"))).await;
        unavailable_with(&answer, UnavailableReason::MissingAnchor);
        // And the foreign task's own row is, of course, readable by it.
        let (r, _) = result_of(&ask(&pool, &other, &by_id("foreign", None)).await);
        assert_eq!(r.capture, json!("SECRET OF B"));
    }

    #[tokio::test]
    async fn a_link_that_leads_into_another_task_is_a_history_gap_with_no_foreign_content() {
        let pool = connect_in_memory().await.unwrap();
        let mine = new_task(&pool).await;
        put_state(&pool, &mine, json!({})).await;
        let other = new_task_in(&pool, "Other", &[]).await;
        // The only `s`/`x` result in the system belongs to the other task.
        put_result(
            &pool,
            &other,
            "foreign",
            None,
            "s",
            "x",
            Some(json!("SECRET OF B")),
            1,
        )
        .await;
        // A valid same-task anchor whose predecessor is that foreign row.
        put_result(&pool, &mine, "mine", Some("foreign"), "t", "y", None, 1).await;
        point_at(&pool, &mine, json!("mine")).await;
        for request in [
            by_name("s", Some("x"), None),
            by_name("s", Some("x"), Some("mine")),
            by_id("foreign", Some("mine")),
        ] {
            let answer = ask(&pool, &mine, &request).await;
            // The exact-ID form refuses the foreign ID up front; the stage
            // forms walk into the gap.
            let reason = match request {
                ContextRequest::StageById { .. } => UnavailableReason::MissingResult,
                _ => UnavailableReason::HistoryGap,
            };
            unavailable_with(&answer, reason);
            assert!(
                !format!("{answer:?}").contains("SECRET OF B"),
                "{request:?}"
            );
        }
        // A match found before the gap is still a valid answer.
        let (r, _) = result_of(&ask(&pool, &mine, &by_name("t", None, None)).await);
        assert_eq!(r.result_id, "mine");
        // Walking an anchored exact result past a gap says gap, not "not at or
        // before the anchor".
        put_result(&pool, &mine, "later", Some("mine"), "u", "z", None, 0).await;
        put_result(&pool, &mine, "orphan", Some("pruned"), "u", "z", None, 0).await;
        let answer = ask(&pool, &mine, &by_id("mine", Some("orphan"))).await;
        unavailable_with(&answer, UnavailableReason::HistoryGap);
    }

    #[tokio::test]
    async fn pruned_and_corrupt_history_is_unavailable_never_a_snapshot_or_a_hang() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        put_state(
            &pool,
            &task,
            json!({"stages": {"s": "STALE SNAPSHOT"}, "current_result_id": "pruned"}),
        )
        .await;
        // The pointer names a pruned event: no substitution from `stages`.
        let answer = ask(&pool, &task, &by_name("s", None, None)).await;
        assert_eq!(
            unavailable_with(&answer, UnavailableReason::MissingAnchor).as_deref(),
            Some("pruned")
        );
        assert!(!format!("{answer:?}").contains("STALE"));
        // An exact result needs no pointer at all.
        put_result(&pool, &task, "r1", None, "s", "x", Some(json!(1)), 1).await;
        assert_eq!(
            result_of(&ask(&pool, &task, &by_id("r1", None)).await)
                .0
                .result_id,
            "r1"
        );

        // A cycle terminates as malformed.
        put_result(&pool, &task, "c1", Some("c2"), "a", "x", None, 1).await;
        put_result(&pool, &task, "c2", Some("c1"), "b", "x", None, 1).await;
        point_at(&pool, &task, json!("c1")).await;
        let answer = ask(&pool, &task, &by_name("zzz", None, None)).await;
        unavailable_with(&answer, UnavailableReason::MalformedHistory);
        let answer = ask(&pool, &task, &by_id("r1", Some("c1"))).await;
        unavailable_with(&answer, UnavailableReason::MalformedHistory);
        // A self-link too.
        put_result(&pool, &task, "me", Some("me"), "a", "x", None, 1).await;
        let answer = ask(&pool, &task, &by_name("zzz", None, Some("me"))).await;
        unavailable_with(&answer, UnavailableReason::MalformedHistory);

        // Result rows this version could not have written.
        for (id, payload) in [
            ("bad-null", json!(null)),
            ("bad-shape", json!({"source_stage": "s"})),
            (
                "bad-link",
                json!({"source_stage":"a","source_kind":"shell","outcome":"x",
                       "destination_stage":"b","capture_present":false,"capture":null,
                       "previous_result_id": 5}),
            ),
        ] {
            put_raw_event(&pool, &task, id, "stage_result", None, payload, 1).await;
            for request in [by_id(id, None), by_name("a", None, Some(id))] {
                let answer = ask(&pool, &task, &request).await;
                unavailable_with(&answer, UnavailableReason::MalformedHistory);
            }
        }
        // A malformed predecessor mid-chain.
        put_result(&pool, &task, "top", Some("bad-shape"), "a", "x", None, 1).await;
        let answer = ask(&pool, &task, &by_name("nothing", None, Some("top"))).await;
        unavailable_with(&answer, UnavailableReason::MalformedHistory);
        // A malformed pointer is never a legacy boundary.
        for bad in [json!(""), json!(7), json!([]), json!({}), json!(false)] {
            point_at(&pool, &task, bad.clone()).await;
            let answer = ask(&pool, &task, &by_name("s", None, None)).await;
            unavailable_with(&answer, UnavailableReason::MalformedHistory);
            assert!(!format!("{answer:?}").contains("STALE"), "{bad}");
        }
    }

    /// `terminal` never departs and `not_a_kind` is no kind at all: a row
    /// claiming either was not written by this engine, whichever way the
    /// lookup reaches it.
    #[tokio::test]
    async fn a_result_with_a_source_kind_that_cannot_depart_is_malformed_history() {
        let pool = connect_in_memory().await.unwrap();
        for (n, bad_kind) in ["terminal", "not_a_kind", ""].into_iter().enumerate() {
            let task = new_task_in(&pool, &format!("kinds-{n}"), &[]).await;
            put_state(&pool, &task, json!({})).await;
            let (bad, good) = (format!("bad-{n}"), format!("good-{n}"));
            let payload = json!({"source_stage": "s", "source_kind": bad_kind, "outcome": "x",
                                 "destination_stage": "next", "capture_present": true,
                                 "capture": "PRIVATE-CAPTURE", "previous_result_id": null});
            put_raw_event(&pool, &task, &bad, "stage_result", None, payload, 1).await;
            // A good result whose predecessor is the bad row.
            put_result(&pool, &task, &good, Some(&bad), "g", "x", None, 0).await;
            for request in [
                // As an exact result.
                by_id(&bad, None),
                // As an explicit anchor.
                by_name("s", None, Some(&bad)),
                // As an encountered predecessor, for requests that must read it.
                by_name("s", None, Some(&good)),
                by_id(&bad, Some(&good)),
            ] {
                let answer = ask(&pool, &task, &request).await;
                unavailable_with(&answer, UnavailableReason::MalformedHistory);
                assert!(
                    !format!("{answer:?}").contains("PRIVATE-CAPTURE"),
                    "{bad_kind:?}: {request:?}"
                );
            }
            // As the implicit anchor, through the current pointer.
            point_at(&pool, &task, json!(bad)).await;
            let answer = ask(&pool, &task, &by_name("s", None, None)).await;
            unavailable_with(&answer, UnavailableReason::MalformedHistory);
            assert!(!format!("{answer:?}").contains("PRIVATE-CAPTURE"));
        }

        // The five kinds a task can leave are each readable, in the same
        // positions.
        let task = new_task_in(&pool, "kinds-ok", &[]).await;
        put_state(&pool, &task, json!({})).await;
        let mut previous: Option<String> = None;
        for kind in chocofactory_core::context::DEPARTING_KINDS {
            let id = format!("ok-{kind}");
            let payload = StageResultPayload::new(
                kind,
                kind,
                "x",
                "next",
                Some(json!(kind)),
                previous.clone(),
            )
            .to_value();
            put_raw_event(&pool, &task, &id, "stage_result", None, payload, 0).await;
            previous = Some(id);
        }
        point_at(&pool, &task, json!(previous.unwrap())).await;
        for kind in chocofactory_core::context::DEPARTING_KINDS {
            let (r, _) = result_of(&ask(&pool, &task, &by_name(kind, None, None)).await);
            assert_eq!(r.source_kind, kind);
            assert_eq!(r.capture, json!(kind));
        }
    }

    /// A workflow may key an outcome with an empty string; the engine applies
    /// it as written, so the result it wrote must read back.
    #[tokio::test]
    async fn a_result_with_an_empty_outcome_is_returned_not_malformed() {
        let pool = connect_in_memory().await.unwrap();
        let task = new_task(&pool).await;
        put_state(&pool, &task, json!({})).await;
        put_result(&pool, &task, "e1", None, "s", "", Some(json!("kept")), 0).await;
        point_at(&pool, &task, json!("e1")).await;
        let (r, _) = result_of(&ask(&pool, &task, &by_id("e1", None)).await);
        assert_eq!((r.outcome.as_str(), r.capture), ("", json!("kept")));
        let (r, _) = result_of(&ask(&pool, &task, &by_name("s", None, None)).await);
        assert_eq!(r.result_id, "e1");
    }

    #[tokio::test]
    async fn only_a_pointerless_task_falls_back_to_a_snapshot() {
        let pool = connect_in_memory().await.unwrap();
        // Every reachable shape of payload, task and stages.
        let cases: Vec<(&str, Value, Option<Value>)> = vec![
            (
                "object",
                json!({"stages": {"s": {"outcome": "approved", "n": 1}}}),
                Some(json!({"outcome": "approved", "n": 1})),
            ),
            (
                "null pointer",
                json!({"current_result_id": null, "stages": {"s": "text"}}),
                Some(json!("text")),
            ),
            (
                "json null capture",
                json!({"stages": {"s": null}}),
                Some(Value::Null),
            ),
            ("empty object", json!({}), None),
            ("null payload", json!(null), None),
            ("string payload", json!("x"), None),
            ("number payload", json!(7), None),
            ("array payload", json!([{"stages": {"s": 1}}]), None),
            ("null stages", json!({"stages": null}), None),
            ("string stages", json!({"stages": "s"}), None),
            ("array stages", json!({"stages": ["s"]}), None),
            ("other stage only", json!({"stages": {"t": 1}}), None),
            ("task shaped", json!({"task": {"stages": {"s": 1}}}), None),
        ];
        for (label, payload, expected) in cases {
            let task = new_task_in(&pool, label, &[]).await;
            put_state(&pool, &task, payload).await;
            let answer = ask(&pool, &task, &by_name("s", None, None)).await;
            match expected {
                Some(capture) => match answer {
                    Lookup::Found(ContextResponse::Snapshot {
                        stage,
                        capture_present,
                        capture: got,
                        anchor_id,
                        ..
                    }) => {
                        assert_eq!(stage, "s", "{label}");
                        assert!(capture_present, "{label}");
                        assert_eq!(got, capture, "{label}");
                        assert_eq!(anchor_id, None, "{label}");
                    }
                    other => panic!("{label}: {other:?}"),
                },
                None => {
                    unavailable_with(&answer, UnavailableReason::MissingSnapshot);
                }
            }
            // A snapshot can never confirm an applied outcome — not even by
            // an `outcome` key inside the captured value.
            let answer = ask(&pool, &task, &by_name("s", Some("approved"), None)).await;
            // The filter decides before any snapshot is looked for: the
            // answer is the same whether or not a capture exists.
            unavailable_with(&answer, UnavailableReason::UnverifiableSnapshotOutcome);
            // And never for an exact ID or an anchored request.
            let answer = ask(&pool, &task, &by_id("s", None)).await;
            unavailable_with(&answer, UnavailableReason::MissingResult);
            let answer = ask(&pool, &task, &by_name("s", None, Some("s"))).await;
            unavailable_with(&answer, UnavailableReason::MissingAnchor);
        }
    }

    #[tokio::test]
    async fn the_task_source_reports_input_availability_title_and_closes() {
        let pool = connect_in_memory().await.unwrap();
        let ask_task = |task: String| {
            let pool = pool.clone();
            async move {
                match lookup(&pool, &task, &ContextRequest::Task).await.unwrap() {
                    Lookup::Found(ContextResponse::Task {
                        task, anchor_id, ..
                    }) => {
                        assert_eq!(anchor_id, None);
                        task
                    }
                    other => panic!("{other:?}"),
                }
            }
        };
        // Seeded title and input, the numeric closes list.
        let task = new_task(&pool).await;
        put_state(
            &pool,
            &task,
            json!({"task": {"input": "do it\nplease", "title": "Seeded"}}),
        )
        .await;
        let got = ask_task(task).await;
        assert_eq!(got.title, "Seeded");
        assert_eq!(
            (got.input_available, got.input.as_deref()),
            (true, Some("do it\nplease"))
        );
        assert_eq!(got.closes, vec![7, 12]);
        // A valid empty input is available.
        let task = new_task(&pool).await;
        put_state(&pool, &task, json!({"task": {"input": "", "title": ""}})).await;
        let got = ask_task(task).await;
        assert_eq!(
            (got.input_available, got.input.as_deref()),
            (true, Some(""))
        );
        assert_eq!(
            got.title, "Fix the thing",
            "an empty seeded title falls back"
        );
        // Missing, null and wrong-type input are unavailable, whatever else
        // the task has recorded.
        for payload in [
            json!({}),
            json!(null),
            json!([]),
            json!({"task": null}),
            json!({"task": []}),
            json!({"task": {"input": null}}),
            json!({"task": {"input": 7}}),
            json!({"task": {"input": ["a"]}}),
            json!({"task": {"title": 5}}),
        ] {
            let task = new_task_in(&pool, "Plain", &[]).await;
            put_state(&pool, &task, payload.clone()).await;
            put_raw_event(
                &pool,
                &task,
                &format!("m-{task}"),
                "human_message",
                None,
                json!({"text": "TYPED INPUT"}),
                1,
            )
            .await;
            let got = ask_task(task).await;
            assert_eq!(
                (got.input_available, got.input.clone()),
                (false, None),
                "{payload}"
            );
            assert_eq!(got.title, "Plain", "{payload}");
            assert_eq!(got.closes, Vec::<u64>::new());
        }
        // No workflow row at all: the known facts still come back.
        let task = new_task(&pool).await;
        let got = ask_task(task).await;
        assert_eq!(got.title, "Fix the thing");
        assert!(!got.input_available);
        assert_eq!(got.closes, vec![7, 12]);
    }

    #[tokio::test]
    async fn unknown_tasks_and_missing_workflow_state_are_told_apart() {
        let pool = connect_in_memory().await.unwrap();
        assert_eq!(
            ask(&pool, "ghost", &ContextRequest::Task).await,
            Lookup::NoSuchTask
        );
        assert_eq!(
            ask(&pool, "ghost", &by_name("s", None, None)).await,
            Lookup::NoSuchTask
        );
        assert_eq!(
            ask(&pool, "ghost", &by_id("r", None)).await,
            Lookup::NoSuchTask
        );
        let task = new_task(&pool).await;
        // No workflow row: unavailable for a stage lookup...
        let answer = ask(&pool, &task, &by_name("s", None, None)).await;
        assert_eq!(
            unavailable_with(&answer, UnavailableReason::MissingWorkflowState),
            None
        );
        // ...but a valid exact result needs no workflow row.
        put_result(&pool, &task, "r1", None, "s", "x", Some(json!(1)), 1).await;
        assert_eq!(
            result_of(&ask(&pool, &task, &by_id("r1", None)).await)
                .0
                .result_id,
            "r1"
        );
        // A deleted task is unknown again (its events go with it or stay
        // unreachable).
        sqlx::query("DELETE FROM events WHERE task_id = ?")
            .bind(&task)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(&task)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            ask(&pool, &task, &ContextRequest::Task).await,
            Lookup::NoSuchTask
        );
    }

    #[tokio::test]
    async fn a_lookup_changes_nothing_that_is_stored() {
        let pool = connect_in_memory().await.unwrap();
        let task = seeded_chain(&pool).await;
        let before = crate::test_support::dump_tables(&pool).await;
        for request in [
            ContextRequest::Task,
            by_name("s", None, None),
            by_name("s", Some("x"), Some("r2")),
            by_name("nowhere", None, None),
            by_id("r1", None),
            by_id("r3", Some("r2")),
            by_id("ghost", None),
        ] {
            ask(&pool, &task, &request).await;
        }
        assert_eq!(crate::test_support::dump_tables(&pool).await, before);
    }

    /// A file-backed database, so reads and writes use different connections.
    async fn file_pool(max_connections: u32) -> (SqlitePool, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("choco-context-{}.db", uuid::Uuid::new_v4()));
        // Create and migrate once, then reopen with the wanted pool size.
        connect(&path).await.unwrap().close().await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .unwrap();
        (pool, path)
    }

    /// The database runs in SQLite's default rollback-journal mode, where a
    /// transaction that has read holds a shared lock and a writer cannot
    /// commit until it ends. The writer here has no busy timeout, so its one
    /// attempt, made while the lookup is held between its statements,
    /// returns at once either way: refused (the lookup holds a lock across
    /// its statements, so every statement reads one generation) or
    /// committed (the lookup held nothing between statements and would mix
    /// generations). Only the refusal is allowed. After the lookup is
    /// released the same write goes through.
    #[tokio::test]
    async fn a_read_sees_one_database_snapshot_while_a_writer_moves_on() {
        let (pool, path) = file_pool(4).await;
        let task = seeded_chain(&pool).await;
        let writer_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true)
                    .busy_timeout(std::time::Duration::ZERO),
            )
            .await
            .unwrap();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let request = by_name("s", Some("x"), None);
        let reading = lookup_with_pause(&pool, &task, &request, || async move {
            reached_tx.send(()).unwrap();
            release_rx.await.unwrap();
        });
        // One transition: a new result becomes current and the oldest link
        // is pruned, in one transaction.
        let transition = |pool: SqlitePool, task: String| async move {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "UPDATE workflow_state
                 SET payload = json_set(payload, '$.current_result_id', 'r4')
                 WHERE task_id = ?",
            )
            .bind(&task)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM events WHERE id = 'r1'")
                .execute(&mut *tx)
                .await?;
            tx.commit().await
        };
        let controller = async {
            reached_rx.await.unwrap();
            let attempt = transition(writer_pool.clone(), task.clone()).await;
            release_tx.send(()).unwrap();
            attempt
        };
        let (answer, attempt) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(reading, controller)
        })
        .await
        .expect("the held read and the writer attempt both finish");
        assert!(
            attempt.is_err(),
            "the writer committed while the lookup was between its statements"
        );
        // The whole answer comes from the generation the anchor was read in:
        // r3 current, r1 still present.
        let (r, anchor) = result_of(&answer.unwrap());
        assert_eq!(
            (r.result_id.as_str(), anchor.as_deref()),
            ("r1", Some("r3"))
        );
        // Released, the same transition now commits and a fresh request sees
        // the new generation: r4 is the latest match.
        transition(writer_pool.clone(), task.clone()).await.unwrap();
        put_result(
            &pool,
            &task,
            "r4",
            Some("r3"),
            "s",
            "x",
            Some(json!("new")),
            0,
        )
        .await;
        let (r, _) = result_of(&ask(&pool, &task, &request).await);
        assert_eq!(r.result_id, "r4");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_dropped_read_releases_its_connection_and_leaves_nothing() {
        // One connection only: if the dropped read held on to it, nothing
        // else could run.
        let (pool, path) = file_pool(1).await;
        let task = seeded_chain(&pool).await;
        let before = crate::test_support::dump_tables(&pool).await;
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel::<()>();
        let request = by_name("s", Some("x"), None);
        let held = lookup_with_pause(&pool, &task, &request, || async move {
            reached_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        tokio::select! {
            _ = held => panic!("the held read cannot finish"),
            reached = reached_rx => reached.unwrap(),
        }
        // `held` is dropped here. A writer must now get the connection and
        // commit, and a new lookup must work.
        let write = async {
            put_result(&pool, &task, "after-drop", Some("r3"), "s", "y", None, 0).await;
            point_at(&pool, &task, json!("after-drop")).await;
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), write)
            .await
            .expect("the dropped read left the connection locked");
        let (r, _) = result_of(&ask(&pool, &task, &by_name("s", None, None)).await);
        assert_eq!(r.result_id, "after-drop");
        // Nothing but the writer's own two changes is new.
        let after = crate::test_support::dump_tables(&pool).await;
        let changed: Vec<_> = before
            .iter()
            .zip(&after)
            .filter(|(a, b)| a != b)
            .map(|(a, _)| a.0.as_str())
            .collect();
        assert_eq!(changed, ["workflow_state", "events"]);
        let _ = std::fs::remove_file(path);
        let _ = db_events::get;
        let _ = EventType::StageResult;
    }
}
