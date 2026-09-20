use chocofactory_core::models::{Session, SessionEndReason, SessionStatus};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

const COLUMNS: &str = "id, task_id, stage, role, cli_adapter, model, adapter_session_id, status, \
     end_reason, resumed_from, started_at, ended_at";

#[derive(FromRow)]
struct SessionRow {
    id: String,
    task_id: String,
    stage: String,
    role: String,
    cli_adapter: String,
    model: String,
    adapter_session_id: Option<String>,
    status: String,
    end_reason: Option<String>,
    resumed_from: Option<String>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

impl From<SessionRow> for Session {
    fn from(row: SessionRow) -> Self {
        Session {
            id: row.id,
            task_id: row.task_id,
            stage: row.stage,
            role: row.role,
            cli_adapter: row.cli_adapter,
            model: row.model,
            adapter_session_id: row.adapter_session_id,
            status: row
                .status
                .parse()
                .expect("sessions.status holds a value written by this module"),
            // Lenient where `status` above is not, and deliberately so
            // (#69). `status` is load-bearing — a session whose state can't
            // be read is not something to guess at — but `end_reason` is
            // explanatory metadata that is already `None` for most rows, so
            // an unrecognized value degrades to "no specific reason"
            // instead of panicking on a row read.
            //
            // That difference matters the moment a variant is added, as
            // `Cancelled` just was: a DB written by a newer daemon and then
            // read by an older one is otherwise a crash on `SELECT`, in a
            // `From` impl that runs for every row, with no way back short
            // of editing the database by hand.
            end_reason: row.end_reason.and_then(|reason| match reason.parse() {
                Ok(reason) => Some(reason),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "ignoring an unrecognized sessions.end_reason — written by a newer daemon?"
                    );
                    None
                }
            }),
            resumed_from: row.resumed_from,
            started_at: row.started_at,
            ended_at: row.ended_at,
        }
    }
}

pub struct NewSession<'a> {
    pub task_id: &'a str,
    pub stage: &'a str,
    pub role: &'a str,
    pub cli_adapter: &'a str,
    pub model: &'a str,
}

pub async fn create(pool: &SqlitePool, new: NewSession<'_>) -> Result<Session, sqlx::Error> {
    create_inner(pool, new, None).await
}

/// The earlier session this new one continues (#92).
#[derive(Debug, Clone, Copy)]
pub struct ResumedFrom<'a> {
    pub session_id: &'a str,
    pub adapter_session_id: &'a str,
}

/// [`create`] for a session that resumes `from`'s adapter session instead of
/// opening its own (#92). The new row starts out carrying that
/// `adapter_session_id`, so a cancel or a crash before the CLI's first
/// `init` line still has something to point at; `set_adapter_session_id`
/// overwrites it with whatever session the CLI actually continued into.
pub async fn create_resumed(
    pool: &SqlitePool,
    new: NewSession<'_>,
    from: ResumedFrom<'_>,
) -> Result<Session, sqlx::Error> {
    create_inner(pool, new, Some(from)).await
}

async fn create_inner(
    pool: &SqlitePool,
    new: NewSession<'_>,
    from: Option<ResumedFrom<'_>>,
) -> Result<Session, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, SessionRow>(&format!(
        "INSERT INTO sessions (id, task_id, stage, role, cli_adapter, model, adapter_session_id, \
         status, resumed_from, started_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(new.task_id)
    .bind(new.stage)
    .bind(new.role)
    .bind(new.cli_adapter)
    .bind(new.model)
    .bind(from.map(|from| from.adapter_session_id))
    .bind(SessionStatus::Active.to_string())
    .bind(from.map(|from| from.session_id))
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

/// How many consecutive resumes led to `session_id` — 0 for a session that
/// opened its own, 1 for one that resumed such a session, and so on (#92).
///
/// Walks the `resumed_from` chain rather than counting rows for the stage:
/// what needs bounding is how many times *one* interrupted session has been
/// picked up again, and a fresh start in between deliberately resets that.
/// The walk stops at `limit` (the caller's cap) instead of following the
/// chain to its root — the answer is only ever compared against that cap,
/// and stopping there also means a `resumed_from` cycle, which no writer
/// here can create, could not spin this forever.
///
/// A session id with no row is 0, not an error: the only caller passes a
/// session it has just read under the task's lock, so a missing row means
/// the task was deleted underneath it, and the writes that follow fail
/// loudly on their own foreign key rather than needing this to speak for
/// them.
pub async fn resume_chain_len(
    pool: &SqlitePool,
    session_id: &str,
    limit: usize,
) -> Result<usize, sqlx::Error> {
    let mut current = session_id.to_string();
    for walked in 0..limit {
        let parent: Option<Option<String>> =
            sqlx::query_scalar("SELECT resumed_from FROM sessions WHERE id = ?")
                .bind(&current)
                .fetch_optional(pool)
                .await?;
        match parent.flatten() {
            Some(parent) => current = parent,
            None => return Ok(walked),
        }
    }
    Ok(limit)
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Session>, sqlx::Error> {
    let row =
        sqlx::query_as::<_, SessionRow>(&format!("SELECT {COLUMNS} FROM sessions WHERE id = ?"))
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(Into::into))
}

pub async fn list_for_task(pool: &SqlitePool, task_id: &str) -> Result<Vec<Session>, sqlx::Error> {
    let rows = sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {COLUMNS} FROM sessions WHERE task_id = ? ORDER BY id"
    ))
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// The most recent session recorded against `task_id` for `stage` — the
/// session currently "open" for that stage, if any (P1-8 LLD §2.5,
/// `WorkflowEngine::send_message`'s lookup). Primarily ordered by
/// `started_at` (unlike `list_for_task`'s `BY id`, which has no temporal
/// meaning — a random UUID); `id DESC` is only a deterministic tie-break
/// for two rows whose timestamps happen to collide, not a claim that `id`
/// carries ordering meaning of its own. For a stage like `chat.yaml`'s
/// (never re-entered — see `role_config`/`send_message`'s `on: {}`
/// requirement), there's only ever one row to find; both the ordering and
/// the tie-break matter once workflows with re-entrant stages exist.
pub async fn get_current_for_stage(
    pool: &SqlitePool,
    task_id: &str,
    stage: &str,
) -> Result<Option<Session>, sqlx::Error> {
    // `id DESC` is a tie-break, not the primary ordering: SQLite/chrono
    // timestamp precision isn't guaranteed to differ between two rows
    // inserted close together, and without a tie-break that'd make this
    // query's result nondeterministic (`fetch_optional` would pick
    // whichever row the engine happened to visit first) rather than
    // merely "arbitrary among ties" the way an explicit tie-break is.
    let row = sqlx::query_as::<_, SessionRow>(&format!(
        "SELECT {COLUMNS} FROM sessions WHERE task_id = ? AND stage = ? ORDER BY started_at DESC, id DESC LIMIT 1"
    ))
    .bind(task_id)
    .bind(stage)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Persists the CLI's `adapter_session_id` for later resume (§4.1).
pub async fn set_adapter_session_id(
    pool: &SqlitePool,
    id: &str,
    adapter_session_id: &str,
) -> Result<Option<Session>, sqlx::Error> {
    let row = sqlx::query_as::<_, SessionRow>(&format!(
        "UPDATE sessions SET adapter_session_id = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(adapter_session_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Updates `status` and `end_reason` together in a single statement.
/// `end_reason` records why `status` reached its current value, for cases
/// where `status` alone can't tell two paths apart (e.g. the idle reaper's
/// clean exit vs. a turn actually finishing — both land on `Idle`). Setting
/// both fields in one `UPDATE` matters, not just for tidiness: a watcher
/// polling this row from another task must never be able to observe the
/// new `status` with the *previous* `end_reason` still attached, which a
/// pair of separate statements would allow in the gap between them.
/// Callers not setting a specific reason should pass `None`, clearing any
/// stale value left over from an earlier transition (e.g. a resumed
/// session going back to `Active` sheds whatever `end_reason` its last
/// `Idle` had).
pub async fn update_status(
    pool: &SqlitePool,
    id: &str,
    status: SessionStatus,
    ended_at: Option<DateTime<Utc>>,
    end_reason: Option<SessionEndReason>,
) -> Result<Option<Session>, sqlx::Error> {
    let row = sqlx::query_as::<_, SessionRow>(&format!(
        "UPDATE sessions SET status = ?, ended_at = ?, end_reason = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(status.to_string())
    .bind(ended_at)
    .bind(end_reason.map(|reason| reason.to_string()))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Daemon-restart recovery (§4.3): any session left `active` in the DB when
/// the daemon starts is dead — its process is gone. A session with a
/// persisted `adapter_session_id` is flipped to `idle`, ready to `resume`
/// on the next message. A session that crashed before its first
/// `SessionMeta` ever arrived has no `adapter_session_id` to resume from,
/// so it's flipped straight to `exited` instead — landing it in `idle`
/// would strand it in a state `send_message` can never recover from. Call
/// once at startup before any `SessionManager` use.
pub async fn recover_stale_active_sessions(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let idled = sqlx::query(
        "UPDATE sessions SET status = ? WHERE status = ? AND adapter_session_id IS NOT NULL",
    )
    .bind(SessionStatus::Idle.to_string())
    .bind(SessionStatus::Active.to_string())
    .execute(pool)
    .await?
    .rows_affected();

    let exited = sqlx::query(
        "UPDATE sessions SET status = ?, ended_at = ? WHERE status = ? AND adapter_session_id IS NULL",
    )
    .bind(SessionStatus::Exited.to_string())
    .bind(Utc::now())
    .bind(SessionStatus::Active.to_string())
    .execute(pool)
    .await?
    .rows_affected();

    Ok(idled + exited)
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, projects, tasks};
    use serde_json::json;

    async fn seed_task(pool: &SqlitePool) -> String {
        let project_id = projects::create(pool, "demo", None).await.unwrap().id;
        tasks::create(
            pool,
            tasks::NewTask {
                project_id: &project_id,
                workflow_def: "chat",
                title: "T",
                config: json!({}),
                workflow_path: None,
                workflow_sha256: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// #92: a resumed session records the session it continued and starts
    /// out carrying its adapter session, and the chain it forms is what
    /// bounds how many times one session can be picked up again.
    #[tokio::test]
    async fn a_resumed_session_points_back_at_the_session_it_continued() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;
        let new_session = || NewSession {
            task_id: &task_id,
            stage: "coding",
            role: "coder",
            cli_adapter: "claude",
            model: "sonnet",
        };

        let first = create(&pool, new_session()).await.unwrap();
        assert_eq!(first.resumed_from, None);
        assert_eq!(resume_chain_len(&pool, &first.id, 3).await.unwrap(), 0);

        let second = create_resumed(
            &pool,
            new_session(),
            ResumedFrom {
                session_id: &first.id,
                adapter_session_id: "sess-123",
            },
        )
        .await
        .unwrap();
        assert_eq!(second.resumed_from.as_deref(), Some(first.id.as_str()));
        assert_eq!(second.adapter_session_id.as_deref(), Some("sess-123"));
        assert_eq!(second.status, SessionStatus::Active);
        assert_eq!(resume_chain_len(&pool, &second.id, 3).await.unwrap(), 1);

        let third = create_resumed(
            &pool,
            new_session(),
            ResumedFrom {
                session_id: &second.id,
                adapter_session_id: "sess-123",
            },
        )
        .await
        .unwrap();
        assert_eq!(resume_chain_len(&pool, &third.id, 3).await.unwrap(), 2);
        // The walk stops at the caller's limit rather than following the
        // whole chain.
        assert_eq!(resume_chain_len(&pool, &third.id, 1).await.unwrap(), 1);
        // And a session that doesn't exist has no chain rather than an error.
        assert_eq!(resume_chain_len(&pool, "nope", 3).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        let created = create(
            &pool,
            NewSession {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();
        assert_eq!(created.status, SessionStatus::Active);
        assert!(created.adapter_session_id.is_none());
        assert!(!created.id.is_empty());

        let with_session = set_adapter_session_id(&pool, &created.id, "sess-123")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(with_session.adapter_session_id.as_deref(), Some("sess-123"));

        let idle = update_status(&pool, &created.id, SessionStatus::Idle, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idle.status, SessionStatus::Idle);

        let now = Utc::now();
        let exited = update_status(&pool, &created.id, SessionStatus::Exited, Some(now), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exited.status, SessionStatus::Exited);
        assert_eq!(exited.ended_at, Some(now));

        let sessions = list_for_task(&pool, &task_id).await.unwrap();
        assert_eq!(sessions.len(), 1);

        assert!(delete(&pool, &created.id).await.unwrap());
        assert!(get(&pool, &created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn recover_stale_active_sessions_flips_active_to_idle_and_leaves_others_alone() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;
        let new_session = || NewSession {
            task_id: &task_id,
            stage: "chatting",
            role: "chat",
            cli_adapter: "claude",
            model: "sonnet",
        };

        let active = create(&pool, new_session()).await.unwrap();
        set_adapter_session_id(&pool, &active.id, "sess-active")
            .await
            .unwrap();
        let already_idle = create(&pool, new_session()).await.unwrap();
        update_status(&pool, &already_idle.id, SessionStatus::Idle, None, None)
            .await
            .unwrap();
        let exited = create(&pool, new_session()).await.unwrap();
        update_status(
            &pool,
            &exited.id,
            SessionStatus::Exited,
            Some(Utc::now()),
            None,
        )
        .await
        .unwrap();

        let recovered = recover_stale_active_sessions(&pool).await.unwrap();
        assert_eq!(recovered, 1);

        assert_eq!(
            get(&pool, &active.id).await.unwrap().unwrap().status,
            SessionStatus::Idle
        );
        assert_eq!(
            get(&pool, &already_idle.id).await.unwrap().unwrap().status,
            SessionStatus::Idle
        );
        assert_eq!(
            get(&pool, &exited.id).await.unwrap().unwrap().status,
            SessionStatus::Exited
        );
    }

    #[tokio::test]
    async fn recover_stale_active_sessions_exits_sessions_that_never_got_an_adapter_session_id() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        // Simulates a daemon crash before the CLI's first SessionMeta
        // event ever arrived: still `active`, adapter_session_id is still
        // NULL.
        let crashed_before_session = create(
            &pool,
            NewSession {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();
        assert!(crashed_before_session.adapter_session_id.is_none());

        let recovered = recover_stale_active_sessions(&pool).await.unwrap();
        assert_eq!(recovered, 1);

        let session = get(&pool, &crashed_before_session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.status, SessionStatus::Exited);
        assert!(session.ended_at.is_some());
    }

    #[tokio::test]
    async fn get_current_for_stage_returns_none_when_no_session_exists() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;
        assert!(
            get_current_for_stage(&pool, &task_id, "chatting")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_current_for_stage_filters_by_stage_and_picks_the_most_recent() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task(&pool).await;

        let other_stage = create(
            &pool,
            NewSession {
                task_id: &task_id,
                stage: "other",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();

        let first = create(
            &pool,
            NewSession {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = create(
            &pool,
            NewSession {
                task_id: &task_id,
                stage: "chatting",
                role: "chat",
                cli_adapter: "claude",
                model: "sonnet",
            },
        )
        .await
        .unwrap();

        let current = get_current_for_stage(&pool, &task_id, "chatting")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.id, second.id);
        assert_ne!(current.id, first.id);
        assert_ne!(current.id, other_stage.id);
    }
}
