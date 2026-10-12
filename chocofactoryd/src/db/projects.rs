use chocofactory_core::models::Project;
use chrono::Utc;
use sqlx::{FromRow, QueryBuilder, SqlitePool};
use uuid::Uuid;

// A macro rather than a const so `concat!` can build each query as the
// `&'static str` that sqlx 0.9 accepts without `AssertSqlSafe`.
macro_rules! columns {
    () => {
        "id, name, repo_path, created_at, max_agents"
    };
}

#[derive(FromRow)]
struct ProjectRow {
    id: String,
    name: String,
    repo_path: Option<String>,
    created_at: chrono::DateTime<Utc>,
    max_agents: Option<i64>,
}

impl From<ProjectRow> for Project {
    fn from(row: ProjectRow) -> Self {
        Project {
            id: row.id,
            name: row.name,
            repo_path: row.repo_path,
            created_at: row.created_at,
            // 0 is never stored (it means "no cap of its own"), but a
            // hand-edited 0 reads as that too rather than as a pool of zero.
            max_agents: row
                .max_agents
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0),
        }
    }
}

pub async fn create(
    pool: &SqlitePool,
    name: &str,
    repo_path: Option<&str>,
) -> Result<Project, sqlx::Error> {
    create_capped(pool, name, repo_path, None).await
}

/// [`create`] with the project's own agent cap. `Some(0)` and `None` both
/// store NULL: no cap of its own.
pub async fn create_capped(
    pool: &SqlitePool,
    name: &str,
    repo_path: Option<&str>,
    max_agents: Option<u32>,
) -> Result<Project, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let row = sqlx::query_as::<_, ProjectRow>(concat!(
        "INSERT INTO projects (id, name, repo_path, created_at, max_agents) \
         VALUES (?, ?, ?, ?, ?) RETURNING ",
        columns!()
    ))
    .bind(id)
    .bind(name)
    .bind(repo_path)
    .bind(now)
    .bind(max_agents.filter(|n| *n > 0).map(i64::from))
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Project>, sqlx::Error> {
    let row = sqlx::query_as::<_, ProjectRow>(concat!(
        "SELECT ",
        columns!(),
        " FROM projects WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Project>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProjectRow>(concat!(
        "SELECT ",
        columns!(),
        " FROM projects ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Updates `name`/`repo_path` on project `id` — the merge of the old
/// `rename` with a `repo_path` setter (issue #88), as a single `UPDATE ...
/// RETURNING` built with `QueryBuilder` rather than a read-then-write, so
/// there is no window for a concurrent update to be silently clobbered.
///
/// `name: None` leaves the name unchanged. `repo_path` is doubly optional
/// (P1-9-style "absent vs. null" shape, mirrored from the API layer's own
/// `Option<Option<String>>`): outer `None` means "don't touch
/// `repo_path`"; `Some(None)` clears it; `Some(Some(path))` sets it.
///
/// If neither `name` nor `repo_path` is given, this is a no-op read (via
/// `get`) rather than an `UPDATE` with an empty `SET` clause — callers
/// (the API layer) are expected to reject that case as a 400 before
/// calling this, but this function stays well-defined regardless.
///
/// Returns `None` if `id` doesn't exist.
pub async fn update(
    pool: &SqlitePool,
    id: &str,
    name: Option<&str>,
    repo_path: Option<Option<&str>>,
) -> Result<Option<Project>, sqlx::Error> {
    update_capped(pool, id, name, repo_path, None).await
}

/// [`update`] that can also set the project's agent cap in the same
/// statement. `max_agents` is doubly optional like `repo_path`: outer `None`
/// leaves it alone; `Some(None)` and `Some(Some(0))` remove the project's
/// own cap; `Some(Some(n))` sets it.
pub async fn update_capped(
    pool: &SqlitePool,
    id: &str,
    name: Option<&str>,
    repo_path: Option<Option<&str>>,
    max_agents: Option<Option<u32>>,
) -> Result<Option<Project>, sqlx::Error> {
    if name.is_none() && repo_path.is_none() && max_agents.is_none() {
        return get(pool, id).await;
    }

    let mut qb = QueryBuilder::new("UPDATE projects SET ");
    let mut first = true;
    if let Some(name) = name {
        qb.push("name = ").push_bind(name.to_string());
        first = false;
    }
    if let Some(repo_path) = repo_path {
        if !first {
            qb.push(", ");
        }
        qb.push("repo_path = ")
            .push_bind(repo_path.map(|s| s.to_string()));
        first = false;
    }
    if let Some(max_agents) = max_agents {
        if !first {
            qb.push(", ");
        }
        qb.push("max_agents = ")
            .push_bind(max_agents.filter(|n| *n > 0).map(i64::from));
    }
    qb.push(" WHERE id = ").push_bind(id.to_string());
    qb.push(concat!(" RETURNING ", columns!()));

    let row = qb
        .build_query_as::<ProjectRow>()
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

/// Every project that has its own agent cap, by id.
pub async fn caps(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, u32>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT id, max_agents FROM projects WHERE max_agents > 0")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, n)| u32::try_from(n).ok().map(|n| (id, n)))
        .collect())
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_in_memory;

    #[tokio::test]
    async fn crud_roundtrip() {
        let pool = connect_in_memory().await.unwrap();

        let created = create(&pool, "demo", None).await.unwrap();
        assert_eq!(created.name, "demo");
        assert!(created.repo_path.is_none());
        assert!(!created.id.is_empty());

        let fetched = get(&pool, &created.id).await.unwrap().unwrap();
        assert_eq!(fetched, created);

        let renamed = update(&pool, &created.id, Some("renamed"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "renamed");

        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "renamed");

        assert!(delete(&pool, &created.id).await.unwrap());
        assert!(get(&pool, &created.id).await.unwrap().is_none());
        assert!(!delete(&pool, &created.id).await.unwrap());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let pool = connect_in_memory().await.unwrap();
        assert!(get(&pool, "does-not-exist").await.unwrap().is_none());
    }

    /// #88: `repo_path` round-trips through `create`/`get`/`list` like any
    /// other column.
    #[tokio::test]
    async fn create_get_list_round_trip_repo_path() {
        let pool = connect_in_memory().await.unwrap();

        let created = create(&pool, "demo", Some("/repo/demo")).await.unwrap();
        assert_eq!(created.repo_path.as_deref(), Some("/repo/demo"));

        let fetched = get(&pool, &created.id).await.unwrap().unwrap();
        assert_eq!(fetched.repo_path.as_deref(), Some("/repo/demo"));

        let all = list(&pool).await.unwrap();
        assert_eq!(all[0].repo_path.as_deref(), Some("/repo/demo"));
    }

    /// `update` changes only the field(s) actually given — a plain rename
    /// must not touch a project's `repo_path`, and vice versa.
    #[tokio::test]
    async fn update_changes_only_the_provided_fields() {
        let pool = connect_in_memory().await.unwrap();
        let created = create(&pool, "demo", Some("/repo/demo")).await.unwrap();

        let renamed = update(&pool, &created.id, Some("renamed"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "renamed");
        assert_eq!(renamed.repo_path.as_deref(), Some("/repo/demo"));

        let repointed = update(&pool, &created.id, None, Some(Some("/repo/new")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repointed.name, "renamed");
        assert_eq!(repointed.repo_path.as_deref(), Some("/repo/new"));
    }

    /// `Some(None)` is the explicit "clear it" shape, distinct from `None`
    /// ("leave it alone") — the same "absent vs. null" distinction the API
    /// layer's `PATCH` body has to make.
    #[tokio::test]
    async fn update_with_some_none_clears_repo_path() {
        let pool = connect_in_memory().await.unwrap();
        let created = create(&pool, "demo", Some("/repo/demo")).await.unwrap();

        let cleared = update(&pool, &created.id, None, Some(None))
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.repo_path.is_none());
        assert_eq!(cleared.name, "demo");
    }

    #[tokio::test]
    async fn update_on_an_unknown_id_is_none() {
        let pool = connect_in_memory().await.unwrap();
        assert!(
            update(&pool, "does-not-exist", Some("x"), None)
                .await
                .unwrap()
                .is_none()
        );
    }
}
