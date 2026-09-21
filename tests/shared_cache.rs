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

/// A statement no other test or run has described before.
fn unseen_statement() -> String {
    format!(
        "select {}::int as n, 'shared'::text as label",
        std::process::id()
    )
}

#[tokio::test]
async fn a_second_connection_does_not_probe_again() {
    let target = require_cluster!();
    let shapes = Arc::new(SharedShapes::shared());
    let conn_str = common::start_proxy_with_shapes(&target, shapes.clone()).await;
    let sql = unseen_statement();

    let first = common::connect_to(&conn_str).await;
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
