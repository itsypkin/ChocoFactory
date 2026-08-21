use chokofactory_core::models::Task;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, QueryBuilder, SqlitePool};
use uuid::Uuid;

const COLUMNS: &str = "id, project_id, parent_task_id, workflow_def, title, status, config, \
     worktree_repo, worktree_project, created_at, updated_at";

#[derive(FromRow)]
struct TaskRow {
    id: String,
    project_id: String,
    parent_task_id: Option<String>,
    workflow_def: String,
    title: String,
    status: String,
    config: Json<Value>,
    worktree_repo: Option<String>,
    worktree_project: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<TaskRow> for Task {
    fn from(row: TaskRow) -> Self {
        Task {
            id: row.id,
            project_id: row.project_id,
            parent_task_id: row.parent_task_id,
            workflow_def: row.workflow_def,
            title: row.title,
            status: row.status,
            config: row.config.0,
            worktree_repo: row.worktree_repo,
            worktree_project: row.worktree_project,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

pub struct NewTask<'a> {
    pub project_id: &'a str,
    pub parent_task_id: Option<&'a str>,
    pub workflow_def: &'a str,
    pub title: &'a str,
    pub config: Value,
}

pub async fn create(pool: &SqlitePool, new: NewTask<'_>) -> Result<Task, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "INSERT INTO tasks (id, project_id, parent_task_id, workflow_def, title, status, config, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'open', ?, ?, ?)
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(new.project_id)
    .bind(new.parent_task_id)
    .bind(new.workflow_def)
    .bind(new.title)
    .bind(Json(new.config))
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Task>, sqlx::Error> {
    let row = sqlx::query_as::<_, TaskRow>(&format!("SELECT {COLUMNS} FROM tasks WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

/// Lists tasks, optionally filtered by project and/or status (§6.2's
/// `choco task list [--project <p>] [--status <s>]`).
pub async fn list(
    pool: &SqlitePool,
    project_id: Option<&str>,
    status: Option<&str>,
) -> Result<Vec<Task>, sqlx::Error> {
    let mut qb = QueryBuilder::new(format!("SELECT {COLUMNS} FROM tasks"));
    let mut has_where = false;
    if let Some(project_id) = project_id {
        qb.push(" WHERE project_id = ")
            .push_bind(project_id.to_string());
        has_where = true;
    }
    if let Some(status) = status {
        qb.push(if has_where {
            " AND status = "
        } else {
            " WHERE status = "
        });
        qb.push_bind(status.to_string());
    }
    qb.push(" ORDER BY id");
    let rows = qb.build_query_as::<TaskRow>().fetch_all(pool).await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn update_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
) -> Result<Option<Task>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "UPDATE tasks SET status = ?, updated_at = ? WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(status)
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Merges `patch` into `tasks.config` (RFC 7396 JSON Merge Patch) and
/// returns the updated task, or `None` if no task has `id`.
///
/// The merge happens *inside* the `UPDATE` via SQLite's `json_patch`
/// rather than as a read-modify-write in Rust, and that is the whole
/// point of this function. Reading the row, merging here, and writing the
/// result back would be a lost update: two concurrent patches would each
/// merge onto the same stale base and the second writer would silently
/// discard the first one's keys. As a single statement there is no window
/// to interleave — SQLite applies the merge to whatever the row holds at
/// write time, so concurrent patches of different roles compose instead
/// of clobbering (regression test:
/// `concurrent_merges_of_different_roles_all_survive`).
///
/// `json_patch` merges objects recursively, so patching
/// `{"roles":{"coder":{...}}}` leaves a sibling `roles.reviewer` and the
/// task-wide `cwd` untouched. Per RFC 7396 a `null` value *removes* that
/// key, which is how an override gets cleared.
///
/// `patch` must be a JSON object. A scalar or array would make
/// `json_patch` replace the entire column rather than merge into it, so
/// callers are responsible for rejecting that (`api::tasks::update_config`
/// returns 400); this layer documents the requirement rather than
/// re-checking it, matching how `create` trusts its `config` too.
pub async fn merge_config(
    pool: &SqlitePool,
    id: &str,
    patch: Value,
) -> Result<Option<Task>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "UPDATE tasks SET config = json_patch(config, ?), updated_at = ? \
         WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(Json(patch))
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Records the repo path and project name `worktree::ensure` actually used
/// to create this task's worktree (§5.5 Q7, issue #58). Called once, from
/// `WorkflowEngine::start_task`, right after `ensure` succeeds — see
/// `Task::worktree_repo`'s doc comment for why every later lookup
/// (`engine::working_dir`, terminal-stage removal) must read this snapshot
/// rather than re-deriving from `config.cwd`/the project's current name.
pub async fn set_worktree(
    pool: &SqlitePool,
    id: &str,
    repo: &str,
    project: &str,
) -> Result<Option<Task>, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, TaskRow>(&format!(
        "UPDATE tasks SET worktree_repo = ?, worktree_project = ?, updated_at = ? \
         WHERE id = ? RETURNING {COLUMNS}"
    ))
    .bind(repo)
    .bind(project)
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM tasks WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, projects};
    use serde_json::json;

    async fn seed_project(pool: &SqlitePool) -> String {
        projects::create(pool, "demo").await.unwrap().id
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let pool = connect_in_memory().await.unwrap();
        let project_id = seed_project(&pool).await;

        let created = create(
            &pool,
            NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: "chat",
                title: "Investigate flaky test",
                config: json!({"model": "sonnet"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(created.status, "open");
        assert_eq!(created.config["model"], "sonnet");
        assert!(!created.id.is_empty());

        let fetched = get(&pool, &created.id).await.unwrap().unwrap();
        assert_eq!(fetched, created);

        let updated = update_status(&pool, &created.id, "closed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "closed");
        assert!(updated.updated_at >= created.updated_at);

        let reconfigured = merge_config(&pool, &created.id, json!({"model": "opus"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reconfigured.config["model"], "opus");

        assert!(delete(&pool, &created.id).await.unwrap());
        assert!(get(&pool, &created.id).await.unwrap().is_none());
    }

    /// Seeds a task whose config is `config` and returns its id.
    async fn seed_task_with_config(pool: &SqlitePool, config: Value) -> String {
        let project_id = seed_project(pool).await;
        create(
            pool,
            NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: "multi-role",
                title: "T",
                config,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// The behavior `choco task reconfigure --role-model coder=opus`
    /// depends on: touching one role must not wipe the task-wide `cwd` or
    /// a sibling role's settings.
    #[tokio::test]
    async fn merge_config_preserves_sibling_keys_and_roles() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task_with_config(
            &pool,
            json!({
                "cwd": "/repo",
                "roles": {
                    "coder": { "cli": "claude", "model": "sonnet" },
                    "reviewer": { "model": "sonnet" }
                }
            }),
        )
        .await;

        let updated = merge_config(
            &pool,
            &task_id,
            json!({ "roles": { "coder": { "model": "opus" } } }),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(updated.config["roles"]["coder"]["model"], "opus");
        // Untouched by the patch, so still present:
        assert_eq!(updated.config["cwd"], "/repo");
        assert_eq!(updated.config["roles"]["coder"]["cli"], "claude");
        assert_eq!(updated.config["roles"]["reviewer"]["model"], "sonnet");
    }

    #[tokio::test]
    async fn merge_config_creates_the_roles_key_when_the_config_is_empty() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task_with_config(&pool, json!({})).await;

        let updated = merge_config(
            &pool,
            &task_id,
            json!({ "roles": { "coder": { "model": "opus" } } }),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(updated.config["roles"]["coder"]["model"], "opus");
    }

    /// RFC 7396: a `null` value deletes the key rather than storing null.
    #[tokio::test]
    async fn merge_config_removes_a_key_when_patched_with_null() {
        let pool = connect_in_memory().await.unwrap();
        let task_id = seed_task_with_config(
            &pool,
            json!({ "roles": { "coder": { "cli": "claude", "model": "sonnet" } } }),
        )
        .await;

        let updated = merge_config(
            &pool,
            &task_id,
            json!({ "roles": { "coder": { "model": null } } }),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(updated.config["roles"]["coder"].get("model").is_none());
        assert_eq!(updated.config["roles"]["coder"]["cli"], "claude");
    }

    #[tokio::test]
    async fn merge_config_on_an_unknown_id_is_none() {
        let pool = connect_in_memory().await.unwrap();
        assert!(
            merge_config(&pool, "nope", json!({ "cwd": "/x" }))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Regression test for the lost-update race `merge_config`'s
    /// single-statement `json_patch` exists to prevent. Every task patches
    /// a *different* role concurrently, so a correct implementation ends
    /// with all of them; a read-merge-write implementation would drop
    /// whichever ones lost the race.
    ///
    /// Uses a real multi-threaded runtime and a file-backed pool, because a
    /// single-threaded runtime plus an in-memory pool can serialize these
    /// enough to hide the very interleaving being tested.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_merges_of_different_roles_all_survive() {
        let dir = std::env::temp_dir().join(format!("chokofactory-merge-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::connect(&dir.join("db.sqlite")).await.unwrap();

        let task_id = seed_task_with_config(&pool, json!({ "cwd": "/repo" })).await;

        const ROLES: usize = 12;
        let mut handles = Vec::new();
        for i in 0..ROLES {
            let pool = pool.clone();
            let task_id = task_id.clone();
            handles.push(tokio::spawn(async move {
                merge_config(
                    &pool,
                    &task_id,
                    json!({ "roles": { format!("role{i}"): { "model": format!("model{i}") } } }),
                )
                .await
            }));
        }
        // Collected rather than unwrapped in the loop, so a failing merge
        // doesn't skip the cleanup below either.
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await);
        }
        let task = get(&pool, &task_id).await;

        // Cleaned up before asserting, so no failure path leaks the temp dir.
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);

        for result in results {
            result
                .expect("merge task panicked")
                .expect("merge_config failed")
                .expect("task vanished mid-merge");
        }
        let task = task.unwrap().unwrap();

        let missing: Vec<String> = (0..ROLES)
            .filter(|i| task.config["roles"][format!("role{i}")]["model"] != format!("model{i}"))
            .map(|i| format!("role{i}"))
            .collect();
        assert!(
            missing.is_empty(),
            "{} of {ROLES} roles were lost — merge is not atomic. missing: {missing:?}, config: {}",
            missing.len(),
            task.config
        );
        // The pre-existing task-wide key survived every merge too.
        assert_eq!(task.config["cwd"], "/repo");
    }

    #[tokio::test]
    async fn list_filters_by_project_and_status() {
        let pool = connect_in_memory().await.unwrap();
        let project_a = seed_project(&pool).await;
        let project_b = seed_project(&pool).await;

        let t1 = create(
            &pool,
            NewTask {
                project_id: &project_a,
                parent_task_id: None,
                workflow_def: "chat",
                title: "A1",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        create(
            &pool,
            NewTask {
                project_id: &project_b,
                parent_task_id: None,
                workflow_def: "chat",
                title: "B1",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        update_status(&pool, &t1.id, "closed").await.unwrap();

        let in_a = list(&pool, Some(&project_a), None).await.unwrap();
        assert_eq!(in_a.len(), 1);
        assert_eq!(in_a[0].title, "A1");

        let closed = list(&pool, None, Some("closed")).await.unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, t1.id);

        let all = list(&pool, None, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn parent_task_id_round_trips() {
        let pool = connect_in_memory().await.unwrap();
        let project_id = seed_project(&pool).await;
        let parent = create(
            &pool,
            NewTask {
                project_id: &project_id,
                parent_task_id: None,
                workflow_def: "coding_task",
                title: "Parent",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        let child = create(
            &pool,
            NewTask {
                project_id: &project_id,
                parent_task_id: Some(&parent.id),
                workflow_def: "chat",
                title: "Child",
                config: json!({}),
            },
        )
        .await
        .unwrap();
        assert_eq!(child.parent_task_id, Some(parent.id));
    }
}
