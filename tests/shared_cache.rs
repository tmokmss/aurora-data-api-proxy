//! What one connection works out, the next one gets.
//!
//! Answering `Describe(statement)` costs five Data API calls. The answer is a
//! property of the schema, not of the connection that asked, so the proxy
//! shares it. These tests are about the two halves of that claim: that the
//! sharing happens, and that it stops happening when the schema moves.
//!
//! The difference is measured rather than asserted about internals. A probe is
//! five round trips and a cache hit is a hash lookup, so the two are three
//! orders of magnitude apart and a generous ratio still separates them.
//!
//! Set `CLUSTER_ARN`, `SECRET_ARN` and `DATABASE` to run these; without them
//! each test returns immediately.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aurora_data_api_proxy::session::SharedShapes;

/// How much faster a cache hit must be than the probe it replaces.
///
/// The real ratio is enormous -- about 900 ms against a fraction of one -- so
/// this only has to be large enough to mean something and small enough never
/// to fire on a slow day.
const SPEEDUP: u32 = 5;

async fn time_prepare(client: &tokio_postgres::Client, sql: &str) -> Duration {
    let start = Instant::now();
    client.prepare(sql).await.expect("prepare");
    start.elapsed()
}

/// Wake a cluster scaled to zero before anything is timed.
///
/// Resuming takes 10-30 seconds and lands on whichever statement happens to go
/// first, which is not a difference between two code paths.
async fn warm_up(client: &tokio_postgres::Client) {
    client.simple_query("select 1").await.expect("warm up");
}

/// A statement no other test or run has described before.
///
/// It carries a placeholder on purpose. Without one the proxy describes it in
/// a single call, and these tests are about the difference between paying for
/// a probe and not paying for it -- which is only visible when the probe is
/// the expensive kind.
fn unseen_statement() -> String {
    format!("select {}::int as n, $1::text as label", std::process::id())
}

#[tokio::test]
async fn a_second_connection_does_not_probe_again() {
    let target = require_cluster!();
    let shapes = Arc::new(SharedShapes::shared());
    let conn_str = common::start_proxy_with_shapes(&target, shapes.clone()).await;
    let sql = unseen_statement();

    let first = common::connect_to(&conn_str).await;
    warm_up(&first).await;
    let cold = time_prepare(&first, &sql).await;
    assert_eq!(
        shapes.len().await,
        1,
        "the first connection should have left its answer where others can find it"
    );

    // A connection of its own, with its own empty per-connection cache: the
    // only way it can be fast is by using what the first one left.
    let second = common::connect_to(&conn_str).await;
    let warm = time_prepare(&second, &sql).await;

    assert!(
        warm * SPEEDUP < cold,
        "a second connection took {warm:?} against the first connection's {cold:?}, \
         which is not the difference between a cache hit and five round trips"
    );
}

#[tokio::test]
async fn ddl_empties_what_every_connection_had_learned() {
    let target = require_cluster!();
    let shapes = Arc::new(SharedShapes::shared());
    let conn_str = common::start_proxy_with_shapes(&target, shapes.clone()).await;

    let client = common::connect_to(&conn_str).await;
    client.prepare(&unseen_statement()).await.expect("prepare");
    assert!(
        !shapes.is_empty().await,
        "something should be cached by now"
    );

    let table = common::unique_table("shared_cache");
    client
        .simple_query(&format!("CREATE TABLE {table} (a int)"))
        .await
        .expect("create");

    assert!(
        shapes.is_empty().await,
        "a statement that reshapes the schema must not leave stale shapes behind, \
         or a client decodes rows against a description that no longer matches"
    );

    client
        .simple_query(&format!("DROP TABLE {table}"))
        .await
        .expect("drop");
}

#[tokio::test]
async fn a_failed_ddl_statement_leaves_the_cache_alone() {
    let target = require_cluster!();
    let shapes = Arc::new(SharedShapes::shared());
    let conn_str = common::start_proxy_with_shapes(&target, shapes.clone()).await;

    let client = common::connect_to(&conn_str).await;
    client.prepare(&unseen_statement()).await.expect("prepare");
    let before = shapes.len().await;
    assert!(before > 0);

    // Nothing changed, so nothing needs forgetting.
    client
        .simple_query("DROP TABLE a_table_that_is_not_there")
        .await
        .expect_err("dropping a table that does not exist should fail");

    assert_eq!(
        shapes.len().await,
        before,
        "DDL that failed changed nothing, so it should cost nothing"
    );
}

#[tokio::test]
async fn describe_cache_connection_keeps_each_connection_to_itself() {
    let target = require_cluster!();
    // What `--describe-cache connection` builds.
    let shapes = Arc::new(SharedShapes::disabled());
    let conn_str = common::start_proxy_with_shapes(&target, shapes.clone()).await;
    let sql = unseen_statement();

    let first = common::connect_to(&conn_str).await;
    warm_up(&first).await;
    let cold = time_prepare(&first, &sql).await;
    assert!(
        shapes.is_empty().await,
        "a disabled store must not be holding anything"
    );

    let second = common::connect_to(&conn_str).await;
    let still_cold = time_prepare(&second, &sql).await;

    assert!(
        still_cold * SPEEDUP > cold,
        "the second connection took {still_cold:?} against {cold:?}: it should have \
         probed for itself rather than finding the first connection's answer"
    );

    // The same connection asking twice is still free; only the sharing is off.
    let repeat = time_prepare(&first, &sql).await;
    assert!(
        repeat * SPEEDUP < cold,
        "a connection should still remember its own answer: {repeat:?} against {cold:?}"
    );
}

/// A statement with no placeholders needs one Data API call, not five.
///
/// The five exist to substitute typed `NULL`s for the placeholders. With none
/// to substitute, the shape query stands alone. The difference is measured
/// against a statement that does have a placeholder and so cannot take the
/// short way: both are unseen, both are described once, and at this distance
/// five round trips are not mistakable for one.
#[tokio::test]
async fn a_statement_without_placeholders_is_described_in_one_call() {
    let target = require_cluster!();
    let shapes = Arc::new(SharedShapes::shared());
    let conn_str = common::start_proxy_with_shapes(&target, shapes.clone()).await;
    let client = common::connect_to(&conn_str).await;
    warm_up(&client).await;

    let id = std::process::id();
    let plain = format!("select {id}::int as n, 'one call'::text as label");
    let parameterised = format!("select {id}::int as n, $1::text as label");

    let one_call = time_prepare(&client, &plain).await;
    let five_calls = time_prepare(&client, &parameterised).await;

    assert!(
        one_call * 3 < five_calls,
        "a statement with no placeholders took {one_call:?} against {five_calls:?} for one \
         with a placeholder: that is not the difference between one round trip and five"
    );
}

/// The short way must describe the statement as exactly as the long way did.
#[tokio::test]
async fn the_one_call_answer_is_the_same_answer() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    let stmt = client
        .prepare("select 1::int4 as n, 'x'::text as label, now() as at")
        .await
        .expect("prepare");

    let columns: Vec<_> = stmt.columns().iter().map(|c| c.name()).collect();
    assert_eq!(columns, vec!["n", "label", "at"]);

    let types: Vec<_> = stmt.columns().iter().map(|c| c.type_().name()).collect();
    assert_eq!(types, vec!["int4", "text", "timestamptz"]);
    assert!(
        stmt.params().is_empty(),
        "there are no placeholders to report"
    );

    // And it still runs, against the description it was given.
    let row = client.query_one(&stmt, &[]).await.expect("execute");
    assert_eq!(row.get::<_, i32>("n"), 1);
    assert_eq!(row.get::<_, &str>("label"), "x");
}

/// Inside a transaction the short way is not taken, because a statement that
/// does not compile would abort everything the caller has done.
#[tokio::test]
async fn a_bad_describe_inside_a_transaction_is_still_survivable() {
    let target = require_cluster!();
    let client = common::connect(&target).await;

    client.simple_query("BEGIN").await.expect("begin");
    client
        .simple_query("CREATE TEMP TABLE probe_guard (a int)")
        .await
        .expect("temp table");

    // No placeholders, so this is exactly the shape the short way would take.
    client
        .prepare("select * from a_table_that_is_not_there")
        .await
        .expect_err("describing nonsense should fail");

    // The transaction is still usable, which it would not be had the failed
    // statement been run inside it without a savepoint.
    let rows = client
        .simple_query("SELECT count(*) FROM probe_guard")
        .await
        .expect("the transaction should have survived");
    assert!(!rows.is_empty());
    client.simple_query("ROLLBACK").await.expect("rollback");
}
