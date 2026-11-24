//! 0019 points existing tasks that already have a pull-request link at the
//! stage the old `/pull/` query would have picked. The migration runs here the
//! way the daemon runs it: 0001–0018 already applied and recorded, then
//! `MIGRATOR.run`.

use std::str::FromStr;

use chocofactory_core::models::REVIEW_STAGE_KEY;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::MIGRATOR;

const REVIEW_MIGRATION: i64 = 19;

/// What the migration must do to a row: nothing (`None`), or point at `key`
/// and set that stage's `number` and `label`.
type Expect = Option<(&'static str, u64)>;

struct Case {
    id: &'static str,
    /// The payload as raw JSON text, in this key order.
    raw: String,
    expect: Expect,
}

fn case(id: &'static str, raw: &str, expect: Expect) -> Case {
    Case {
        id,
        raw: raw.to_string(),
        expect,
    }
}

fn cases() -> Vec<Case> {
    let pull = |n: &str| format!("https://github.com/o/r/pull/{n}");
    vec![
        // The built-in workflow's capture.
        case(
            "built-in",
            &format!(
                r#"{{"stages":{{"open_pr":{{"number":17,"url":"{}"}}}}}}"#,
                pull("17")
            ),
            Some(("open_pr", 17)),
        ),
        case(
            "url-only",
            &format!(r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#, pull("7")),
            Some(("open_pr", 7)),
        ),
        case(
            "trailing-slash",
            &format!(r#"{{"stages":{{"open_pr":{{"url":"{}/"}}}}}}"#, pull("8")),
            Some(("open_pr", 8)),
        ),
        // The URL's number wins over the captured one.
        case(
            "conflicting-number",
            &format!(
                r#"{{"stages":{{"open_pr":{{"number":99,"url":"{}"}}}}}}"#,
                pull("42")
            ),
            Some(("open_pr", 42)),
        ),
        case(
            "not-a-number",
            &format!(r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#, pull("abc")),
            None,
        ),
        // LIKE finds it, the case-sensitive check refuses it.
        case(
            "upper-case-path",
            r#"{"stages":{"open_pr":{"url":"https://github.com/o/r/PULL/5"}}}"#,
            None,
        ),
        // The first candidate is frozen before it is checked: the valid
        // second one is not used in its place.
        case(
            "first-malformed-then-valid",
            &format!(
                r#"{{"stages":{{"a_bad":{{"url":"{}"}},"b_ok":{{"url":"{}"}}}}}}"#,
                pull("abc"),
                pull("3")
            ),
            None,
        ),
        case(
            "two-valid-alphabetical",
            &format!(
                r#"{{"stages":{{"a_first":{{"url":"{}"}},"b_second":{{"url":"{}"}}}}}}"#,
                pull("1"),
                pull("2")
            ),
            Some(("a_first", 1)),
        ),
        // Stored order is not alphabetical: the first stored wins.
        case(
            "two-valid-reverse-stored",
            &format!(
                r#"{{"stages":{{"z_first":{{"url":"{}"}},"a_second":{{"url":"{}"}}}}}}"#,
                pull("11"),
                pull("12")
            ),
            Some(("z_first", 11)),
        ),
        case(
            "quoted-first-key-then-valid",
            &format!(
                r#"{{"stages":{{"we\"ird":{{"url":"{}"}},"ok":{{"url":"{}"}}}}}}"#,
                pull("4"),
                pull("5")
            ),
            None,
        ),
        case(
            "stages-array",
            &format!(r#"{{"stages":[{{"url":"{}"}}]}}"#, pull("9")),
            None,
        ),
        // Captures that are not objects are not candidates; an object whose
        // url is not text is one, and is refused without falling through.
        case(
            "non-object-captures",
            &format!(
                r#"{{"stages":{{"s":"{{\"url\":\"{u}\"}}","t":["{u}"]}}}}"#,
                u = pull("9")
            ),
            None,
        ),
        case(
            "array-url-then-valid",
            &format!(
                r#"{{"stages":{{"s":"text","t":{{"url":["{u}"]}},"v":{{"url":"{u}"}}}}}}"#,
                u = pull("9")
            ),
            None,
        ),
        case(
            "extra-fields-and-result-pointer",
            &format!(
                r#"{{"current_result_id":"r1","arrived":{{"stage":"x"}},"stages":{{"open_pr":{{"number":1,"url":"{}","extra":{{"a":[1,2]}},"ratio":1.50}},"other":{{"k":"v"}}}},"gone":null}}"#,
                pull("21")
            ),
            Some(("open_pr", 21)),
        ),
        case("no-stages", r#"{"current_result_id":"r1"}"#, None),
        case("empty-payload", "{}", None),
        case(
            "zero",
            &format!(r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#, pull("0")),
            None,
        ),
        case(
            "eighteen-digits",
            &format!(
                r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("123456789012345678")
            ),
            Some(("open_pr", 123_456_789_012_345_678)),
        ),
        case(
            "nineteen-digits",
            &format!(
                r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("1234567890123456789")
            ),
            None,
        ),
        case(
            "leading-zeros-positive",
            &format!(r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#, pull("007")),
            Some(("open_pr", 7)),
        ),
        case(
            "leading-zeros-too-long",
            &format!(
                r#"{{"stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("0000000000000000007")
            ),
            None,
        ),
        case(
            "two-trailing-slashes",
            &format!(r#"{{"stages":{{"open_pr":{{"url":"{}//"}}}}}}"#, pull("5")),
            None,
        ),
        case(
            "query-suffix",
            &format!(
                r#"{{"stages":{{"open_pr":{{"url":"{}?x=1"}}}}}}"#,
                pull("5")
            ),
            None,
        ),
        // A pointer that is present, whatever it holds, is left alone.
        case(
            "pointer-text",
            &format!(
                r#"{{"review_stage":"keep","stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("3")
            ),
            None,
        ),
        case(
            "pointer-null",
            &format!(
                r#"{{"review_stage":null,"stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("3")
            ),
            None,
        ),
        case(
            "pointer-number",
            &format!(
                r#"{{"review_stage":5,"stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("3")
            ),
            None,
        ),
        case(
            "pointer-object",
            &format!(
                r#"{{"review_stage":{{}},"stages":{{"open_pr":{{"url":"{}"}}}}}}"#,
                pull("3")
            ),
            None,
        ),
        case("stages-null", r#"{"stages":null}"#, None),
        case("stages-text", r#"{"stages":"x"}"#, None),
        case("stages-number", r#"{"stages":5}"#, None),
        // The key is addressed exactly, so a dot, a backslash or a newline in
        // it changes that stage rather than creating another.
        case(
            "dotted-key",
            &format!(r#"{{"stages":{{"a.b":{{"url":"{}"}}}}}}"#, pull("31")),
            Some(("a.b", 31)),
        ),
        case(
            "backslash-key",
            &format!(r#"{{"stages":{{"a\\b":{{"url":"{}"}}}}}}"#, pull("32")),
            Some(("a\\b", 32)),
        ),
        case(
            "newline-key",
            &format!(r#"{{"stages":{{"a\nb":{{"url":"{}"}}}}}}"#, pull("33")),
            Some(("a\nb", 33)),
        ),
        case(
            "empty-key",
            &format!(r#"{{"stages":{{"":{{"url":"{}"}}}}}}"#, pull("34")),
            Some(("", 34)),
        ),
    ]
}

async fn pool_at_0018() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        );",
    )
    .execute(&pool)
    .await
    .unwrap();
    for migration in MIGRATOR.iter().filter(|m| m.version < REVIEW_MIGRATION) {
        sqlx::raw_sql(migration.sql.clone())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES (?1, ?2, TRUE, ?3, -1)",
        )
        .bind(migration.version)
        .bind(&*migration.description)
        .bind(&*migration.checksum)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

async fn seed(pool: &SqlitePool, cases: &[Case]) {
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ('p1', 'demo', '2026-01-01T00:00:00+00:00')")
        .execute(pool)
        .await
        .unwrap();
    for (n, case) in cases.iter().enumerate() {
        sqlx::query(
            "INSERT INTO tasks (id, project_id, workflow_def, title, created_at, updated_at)
             VALUES (?1, 'p1', 'w', ?1, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
        )
        .bind(case.id)
        .execute(pool)
        .await
        .unwrap();
        // Columns the migration must not touch carry distinct, non-default values.
        sqlx::query(
            "INSERT INTO workflow_state
                 (task_id, current_stage, loop_counters, payload, updated_at,
                  stage_entered_at, stage_kind)
             VALUES (?1, ?2, ?3, ?4, '2026-01-01T00:00:10+00:00', ?5, 'shell')",
        )
        .bind(case.id)
        .bind(format!("stage-{n}"))
        .bind(json!({"gate": {"count": n}}).to_string())
        .bind(&case.raw)
        .bind(format!("2026-01-01T00:00:{n:02}+00:00"))
        .execute(pool)
        .await
        .unwrap();
        // A history the migration must leave exactly as it found it,
        // including a stage result and its predecessor link.
        for (suffix, event_type, payload) in [
            ("1", "stage_entered", json!({"stage": "open_pr"})),
            (
                "2",
                "stage_result",
                json!({"source_stage": "open_pr", "source_kind": "shell", "outcome": "done",
                       "destination_stage": "gate", "capture_present": true,
                       "capture": {"url": "https://github.com/o/r/pull/1"},
                       "previous_result_id": format!("{}-0", case.id)}),
            ),
        ] {
            sqlx::query(
                "INSERT INTO events (id, task_id, session_id, event_type, payload, created_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, '2026-01-01T00:00:05+00:00')",
            )
            .bind(format!("{}-{suffix}", case.id))
            .bind(case.id)
            .bind(event_type)
            .bind(payload.to_string())
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

type StateRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// Every `workflow_state` column but the payload, and all events.
async fn untouched(
    pool: &SqlitePool,
) -> (Vec<StateRow>, Vec<(String, String, String, String, String)>) {
    let state = sqlx::query_as(
        "SELECT task_id, current_stage, loop_counters, updated_at,
                stage_entered_at, stage_kind
           FROM workflow_state ORDER BY task_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let events = sqlx::query_as(
        "SELECT id, task_id, event_type, payload, created_at FROM events ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    (state, events)
}

async fn payloads(pool: &SqlitePool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT task_id, payload FROM workflow_state ORDER BY task_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The payload the migration should leave: `raw` with the pointer, and the
/// pointed stage's `number` and `label`.
fn expected(raw: &str, (key, number): (&str, u64)) -> Value {
    let mut payload: Value = serde_json::from_str(raw).unwrap();
    payload[REVIEW_STAGE_KEY] = json!(key);
    let stage = payload["stages"][key].as_object_mut().unwrap();
    stage.insert("number".into(), json!(number));
    stage.insert("label".into(), json!(format!("#{number}")));
    payload
}

#[tokio::test]
async fn review_stage_migration_points_recognized_links_and_leaves_every_other_row_alone() {
    let cases = cases();
    let pool = pool_at_0018().await;
    seed(&pool, &cases).await;
    let before = untouched(&pool).await;

    MIGRATOR.run(&pool).await.unwrap();

    let after: std::collections::HashMap<String, String> =
        payloads(&pool).await.into_iter().collect();
    for case in &cases {
        let got = &after[case.id];
        match case.expect {
            // An untouched row keeps its raw text, not just its value.
            None => assert_eq!(got, &case.raw, "row {} must be left unchanged", case.id),
            Some(expect) => assert_eq!(
                serde_json::from_str::<Value>(got).unwrap(),
                expected(&case.raw, expect),
                "row {}",
                case.id
            ),
        }
    }
    assert_eq!(untouched(&pool).await, before);

    // Applying the statement again changes nothing: a pointed row is excluded.
    let settled = payloads(&pool).await;
    sqlx::raw_sql(include_str!("../../../migrations/0019_review_stage.sql"))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(payloads(&pool).await, settled);
    assert_eq!(untouched(&pool).await, before);
}
