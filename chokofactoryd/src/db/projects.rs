use chokofactory_core::models::Project;
use chrono::Utc;
use sqlx::{FromRow, SqlitePool};

#[derive(FromRow)]
struct ProjectRow {
    id: i64,
    name: String,
    created_at: chrono::DateTime<Utc>,
}

impl From<ProjectRow> for Project {
    fn from(row: ProjectRow) -> Self {
        Project {
            id: row.id,
            name: row.name,
            created_at: row.created_at,
        }
    }
}

pub async fn create(pool: &SqlitePool, name: &str) -> Result<Project, sqlx::Error> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, ProjectRow>(
        "INSERT INTO projects (name, created_at) VALUES (?, ?) RETURNING id, name, created_at",
    )
    .bind(name)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.into())
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Project>, sqlx::Error> {
    let row =
        sqlx::query_as::<_, ProjectRow>("SELECT id, name, created_at FROM projects WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(Into::into))
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Project>, sqlx::Error> {
    let rows =
        sqlx::query_as::<_, ProjectRow>("SELECT id, name, created_at FROM projects ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn rename(
    pool: &SqlitePool,
    id: i64,
    name: &str,
) -> Result<Option<Project>, sqlx::Error> {
    let row = sqlx::query_as::<_, ProjectRow>(
        "UPDATE projects SET name = ? WHERE id = ? RETURNING id, name, created_at",
    )
    .bind(name)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
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

        let created = create(&pool, "demo").await.unwrap();
        assert_eq!(created.name, "demo");

        let fetched = get(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(fetched, created);

        let renamed = rename(&pool, created.id, "renamed").await.unwrap().unwrap();
        assert_eq!(renamed.name, "renamed");

        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "renamed");

        assert!(delete(&pool, created.id).await.unwrap());
        assert!(get(&pool, created.id).await.unwrap().is_none());
        assert!(!delete(&pool, created.id).await.unwrap());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let pool = connect_in_memory().await.unwrap();
        assert!(get(&pool, 999).await.unwrap().is_none());
    }
}
