//! End-to-end tests with `tokio-postgres`.
//!
//! This driver is the demanding one. It sends `Parse` with no parameter types
//! at all and expects `Describe(statement)` to tell it what they are, then
//! encodes the parameters in binary on the strength of that answer. Everything
//! the proxy does to synthesise a `Describe` is on trial here.
//!
//! Set `CLUSTER_ARN`, `SECRET_ARN` and `DATABASE` to run these; without them
//! each test returns immediately.

mod common;

use tokio_postgres::types::Type;

#[tokio::test]
async fn simple_query_returns_rows() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    let rows = client
        .simple_query("SELECT 1 AS one, 'hi' AS greeting")
        .await
        .unwrap();
    let row = rows
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .expect("a row");
    assert_eq!(row.get("one"), Some("1"));
    assert_eq!(row.get("greeting"), Some("hi"));
}

/// The core of the extended protocol: parameters the client never typed.
#[tokio::test]
async fn extended_query_infers_parameter_types() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    // `$1` has no declared type, so the proxy must work out that it is an
    // integer -- and then the Data API must be given something that is not a
    // string, or PostgreSQL rejects `integer = text`.
    let row = client
        .query_one("SELECT $1::int + 1 AS result", &[&41i32])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>("result"), 42);
}

#[tokio::test]
async fn prepare_reports_parameter_and_result_types() {
    let target = require_cluster!();
    let client = common::connect(&target).await;
    let table = common::unique_table("tp_prep");

    client
        .simple_query(&format!(
            "CREATE TABLE {table} (id serial PRIMARY KEY, name text NOT NULL, amount numeric(12,2))"
        ))
        .await
        .unwrap();

    let stmt = client
        .prepare(&format!(
            "SELECT id, name, amount FROM {table} WHERE id = $1 AND name = $2"
        ))
        .await
        .unwrap();

    // Types come from pg_prepared_statements...
    assert_eq!(stmt.params(), &[Type::INT4, Type::TEXT]);
    // ...and names from the temp table the probe builds without running the
    // query.
    let names: Vec<&str> = stmt.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["id", "name", "amount"]);
    assert_eq!(stmt.columns()[0].type_(), &Type::INT4);
    assert_eq!(stmt.columns()[2].type_(), &Type::NUMERIC);

    client
        .simple_query(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn insert_returning_reports_its_columns() {
    let target = require_cluster!();
    let client = common::connect(&target).await;
    let table = common::unique_table("tp_ret");

    client
        .simple_query(&format!(
            "CREATE TABLE {table} (id serial PRIMARY KEY, name text NOT NULL, amount numeric(12,2))"
        ))
        .await
        .unwrap();

    // The names here are read off the RETURNING clause: there is no way to ask
    // Aurora for them without performing the insert.
    let stmt = client
        .prepare(&format!(
            "INSERT INTO {table}(name) VALUES ($1) RETURNING id, name AS who, amount"
        ))
        .await
        .unwrap();
    let names: Vec<&str> = stmt.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["id", "who", "amount"]);
    // The types are the server's own, from pg_prepared_statements.
    assert_eq!(stmt.columns()[0].type_(), &Type::INT4);
    assert_eq!(stmt.columns()[2].type_(), &Type::NUMERIC);

    // And describing it must not have inserted anything.
    let count: i64 = client
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "describing an INSERT must not perform it");

    // A `serial` column is where the Data API reports its declared type rather
    // than the underlying `int4`; if the proxy took that at face value the
    // value would arrive as text and fail to decode here.
    let row = client.query_one(&stmt, &[&"alpha"]).await.unwrap();
    assert_eq!(row.get::<_, i32>("id"), 1);
    assert_eq!(row.get::<_, &str>("who"), "alpha");

    // Exactly one row, so the statement ran once and only once.
    let count: i64 = client
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1, "the statement must run exactly once");

    client
        .simple_query(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn insert_without_returning_runs_exactly_once() {
    let target = require_cluster!();
    let client = common::connect(&target).await;
    let table = common::unique_table("tp_once");

    client
        .simple_query(&format!(
            "CREATE TABLE {table} (id serial PRIMARY KEY, name text)"
        ))
        .await
        .unwrap();

    // A statement with no result set has no rows to park on the portal, so the
    // command tag is held on the session instead. If that handover were wrong,
    // the insert would happen twice.
    let affected = client
        .execute(&format!("INSERT INTO {table}(name) VALUES ($1)"), &[&"x"])
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let count: i64 = client
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1, "the insert must run exactly once");

    client
        .simple_query(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn round_trips_types_in_binary_format() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    // tokio-postgres asks for binary results, so each of these exercises the
    // proxy's binary encoder against the driver's decoder.
    let row = client
        .query_one(
            "SELECT true AS b, 42::int2 AS i2, 42::int4 AS i4, 42::int8 AS i8, \
                    1.5::float4 AS f4, 1.5::float8 AS f8, 'hi'::text AS t, \
                    '2024-01-15'::date AS d, \
                    '550e8400-e29b-41d4-a716-446655440000'::uuid AS u, \
                    decode('deadbeef','hex') AS by, \
                    ARRAY[1,2,3]::int4[] AS arr, \
                    NULL::text AS n",
            &[],
        )
        .await
        .unwrap();

    assert!(row.get::<_, bool>("b"));
    assert_eq!(row.get::<_, i16>("i2"), 42);
    assert_eq!(row.get::<_, i32>("i4"), 42);
    assert_eq!(row.get::<_, i64>("i8"), 42);
    assert_eq!(row.get::<_, f32>("f4"), 1.5);
    assert_eq!(row.get::<_, f64>("f8"), 1.5);
    assert_eq!(row.get::<_, &str>("t"), "hi");
    assert_eq!(row.get::<_, Vec<u8>>("by"), vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(row.get::<_, Vec<i32>>("arr"), vec![1, 2, 3]);
    assert_eq!(row.get::<_, Option<&str>>("n"), None);
}

/// The Data API strips the zone from a `timestamptz`; if the proxy did not put
/// it back, this value would land an hour or nine out.
#[tokio::test]
async fn timestamptz_keeps_its_instant() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    let row = client
        .query_one(
            "SELECT '2024-01-15 12:34:56+09'::timestamptz AS ts, \
                    extract(epoch from '2024-01-15 12:34:56+09'::timestamptz)::int8 AS epoch",
            &[],
        )
        .await
        .unwrap();
    let epoch: i64 = row.get("epoch");
    // 2024-01-15T03:34:56Z
    assert_eq!(epoch, 1_705_289_696);

    // The text rendering must carry the offset too.
    let text = client
        .simple_query("SELECT '2024-01-15 12:34:56+09'::timestamptz AS ts")
        .await
        .unwrap();
    let row = text
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .expect("a row");
    assert_eq!(row.get("ts"), Some("2024-01-15 03:34:56+00"));
}

#[tokio::test]
async fn transactions_commit_and_roll_back() {
    let target = require_cluster!();
    let mut client = common::connect(&target).await;
    let table = common::unique_table("tp_tx");

    client
        .simple_query(&format!("CREATE TABLE {table} (name text)"))
        .await
        .unwrap();

    let tx = client.transaction().await.unwrap();
    tx.execute(
        &format!("INSERT INTO {table}(name) VALUES ($1)"),
        &[&"kept"],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let tx = client.transaction().await.unwrap();
    tx.execute(
        &format!("INSERT INTO {table}(name) VALUES ($1)"),
        &[&"dropped"],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();

    let names: Vec<String> = client
        .query(&format!("SELECT name FROM {table} ORDER BY name"), &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(names, ["kept"]);

    client
        .simple_query(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn reports_server_errors_with_their_sqlstate() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    let err = client
        .query("SELECT * FROM a_table_that_does_not_exist", &[])
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("a database error");
    assert_eq!(db.code(), &tokio_postgres::error::SqlState::UNDEFINED_TABLE);
    assert!(db.message().contains("does not exist"));
}

/// A failed `Describe` must not take the caller's transaction with it: the
/// probe runs inside a savepoint precisely so that this works.
#[tokio::test]
async fn a_failed_describe_leaves_the_transaction_usable() {
    let target = require_cluster!();
    let mut client = common::connect(&target).await;

    let tx = client.transaction().await.unwrap();
    let err = tx.prepare("SELECT * FROM nothing_here WHERE id = $1").await;
    assert!(err.is_err(), "describing a bad statement should fail");

    let row = tx
        .query_one("SELECT 1 AS still_working", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>("still_working"), 1);
    tx.rollback().await.unwrap();
}

/// The Data API refuses to return `"char"`, which is what psql's `\d` selects.
#[tokio::test]
async fn recovers_from_types_the_data_api_will_not_return() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    let rows = client
        .simple_query("SELECT relkind, relname FROM pg_catalog.pg_class LIMIT 5")
        .await
        .unwrap();
    let count = rows
        .iter()
        .filter(|m| matches!(m, tokio_postgres::SimpleQueryMessage::Row(_)))
        .count();
    assert!(
        count > 0,
        "the query should succeed via the text-cast retry"
    );
}

/// An `interval` cannot come back at all, so it is delivered as text rather
/// than mislabelled as something the client would decode wrongly.
#[tokio::test]
async fn unsupported_types_arrive_as_text() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    let row = client
        .query_one("SELECT '1 day 2 hours'::interval AS i", &[])
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_(), &Type::TEXT);
    assert_eq!(row.get::<_, &str>("i"), "1 day 02:00:00");
}

/// A statement the Data API would silently corrupt is refused instead.
#[tokio::test]
async fn refuses_statements_the_data_api_would_corrupt() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    // Sent as-is, this comes back as `a $1 c`: the Data API rewrites `:b`
    // inside a dollar-quoted string as if it were a bind parameter.
    let err = client
        .simple_query("SELECT $$a :b c$$ AS corrupted")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("a database error");
    assert!(
        db.message().contains("dollar-quoted"),
        "expected an explanation, got: {}",
        db.message()
    );
}
