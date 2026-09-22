//! Working out a statement's shape without running it.
//!
//! `Describe(statement)` arrives before `Bind`, so the proxy has to answer it
//! with no parameter values in hand: the parameter types, and the result
//! columns, of a statement that has never been executed. The Data API offers
//! nothing for this, so the proxy asks PostgreSQL directly.
//!
//! A statement with no placeholders is asked in one call. Wrapped as
//! `SELECT * FROM (<query>) WHERE false`, it comes back with the column names
//! and types and no rows, and nothing about it needs a session to be held open
//! from one call to the next. That is the common case and it is the cheap one.
//!
//! Everything else goes the long way round, through the one stateful thing the
//! Data API does have -- a transaction.
//!
//! Inside a transaction the Data API pins one backend session, which means a
//! `PREPARE` in one call is still there for the next. That gives
//! `pg_prepared_statements`, which has the parameter types and (since
//! PostgreSQL 16) the result types. It does not have the result column *names*,
//! and those come from one of two places:
//!
//! * A statement that only reads is wrapped as
//!   `SELECT * FROM (<query>) WHERE false`, with typed `NULL`s standing in for
//!   the parameters. PostgreSQL plans that as a one-time false filter and never
//!   runs the inner query, while the Data API still returns the column metadata
//!   in full.
//! * `INSERT`/`UPDATE`/`DELETE ... RETURNING` cannot be wrapped that way, so
//!   the names are read off the `RETURNING` clause. The alternatives were both
//!   checked against the live service and both fail:
//!   `CREATE TABLE ... AS EXECUTE` refuses a statement that is not a `SELECT`,
//!   and wrapping the write in a data-modifying CTE to get around that
//!   *performs the write* even with `WITH NO DATA`.
//!
//! The whole probe runs inside a savepoint. A statement that fails inside a
//! PostgreSQL transaction aborts all of it, and a `Describe` of a statement
//! that does not compile is an ordinary thing for a client to do -- it must not
//! take the caller's transaction down with it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pgwire::api::Type;
use pgwire::api::portal::Format;
use pgwire::api::results::{FieldFormat, FieldInfo};
use pgwire::error::ErrorInfo;

use crate::dataapi::Outcome;
use crate::exec;
use crate::session::{Session, StatementShape};
use crate::sql::{self, ReturningItem};
use crate::types;

/// Distinguishes the objects one probe creates from another's.
static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let n = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
    // The pid keeps two proxies against one cluster from colliding.
    format!("{}_{}", std::process::id(), n)
}

/// How the probe's transaction has to be cleaned up afterwards.
enum Cleanup {
    /// The probe opened its own transaction and must roll it back.
    OwnedTransaction,
    /// The probe borrowed the caller's transaction behind a savepoint.
    Savepoint(String),
}

/// Determine a statement's parameter types and result columns.
///
/// The answer is cached per connection, keyed by the SQL text: a client that
/// prepares the same statement repeatedly pays for the probe once.
pub async fn describe_statement(
    session: &Session,
    sql: &str,
) -> Result<Arc<StatementShape>, ErrorInfo> {
    if let Some(cached) = session.cached_shape(sql).await {
        return Ok(cached);
    }

    let (shape, shareable) = if one_call_is_enough(session, sql).await {
        let fields = select_fields(session, sql, &[], None).await?;
        (
            StatementShape {
                param_types: Vec::new(),
                fields,
            },
            true,
        )
    } else {
        run_probe(session, sql).await?
    };

    let shape = Arc::new(shape);
    session
        .cache_shape(sql.to_string(), shape.clone(), shareable)
        .await;
    Ok(shape)
}

/// Whether the shape can be had in one Data API call instead of five.
///
/// The five exist to break a circle. The shape query needs typed `NULL`s where
/// the placeholders are; their types come from `PREPARE`; and a `PREPARE` only
/// survives from one Data API call to the next inside a transaction, which
/// then has to be opened and rolled back. A statement with no placeholders is
/// not in that circle at all: there is nothing to substitute, so the shape
/// query can be asked on its own and it answers with the column names and
/// types together.
///
/// Four things have to hold, and the last two are about safety rather than
/// arithmetic:
///
/// * **No `$n` placeholders.** Otherwise there is something to substitute, and
///   the types to substitute with are what the other four calls are for.
/// * **It only reads.** A write cannot be wrapped in a subquery, and wrapping
///   it in a data-modifying CTE performs the write.
/// * **It opens with a keyword a subquery can hold.** `SHOW`, `EXPLAIN` and
///   the rest are rejected by the wrap -- and by `PREPARE` too, so nothing is
///   lost by leaving them on the path that already handles them.
/// * **The caller has no transaction open.** Inside one, a statement that does
///   not compile aborts the whole transaction, which is exactly what the full
///   probe's savepoint is there to prevent. Outside one, a shape query that
///   fails costs nothing but itself.
///
/// Verified against Aurora PostgreSQL 17.9: run this way, outside any
/// transaction, the wrap returns full column metadata -- names and types, for
/// plain selects, catalogue tables, expression aliases, duplicate column names,
/// CTEs and types the proxy maps to text -- and no rows.
async fn one_call_is_enough(session: &Session, sql: &str) -> bool {
    shape_query_stands_alone(sql) && session.transaction_id().await.is_none()
}

/// The part of [`one_call_is_enough`] that depends only on the statement.
///
/// Split out so that the three conditions on the text can be checked without a
/// cluster, and so checked wherever the tests run rather than only where one
/// is configured.
fn shape_query_stands_alone(sql: &str) -> bool {
    const WRAPPABLE: [&str; 4] = ["SELECT", "WITH", "VALUES", "TABLE"];

    sql::rewrite_placeholders(sql).param_count == 0
        && !sql::modifies_data(sql)
        && sql::leading_words(sql, 1)
            .first()
            .is_some_and(|word| WRAPPABLE.contains(&word.as_str()))
}

/// Work out a shape, and say whether the answer belongs to the schema.
///
/// A probe that opened its own transaction saw the database the way every
/// connection sees it. A probe that borrowed the caller's transaction did not:
/// a `SET LOCAL`, a temporary table or DDL the caller has not committed are all
/// visible inside it and to nobody else, so its answer cannot be lent out.
async fn run_probe(session: &Session, sql: &str) -> Result<(StatementShape, bool), ErrorInfo> {
    let suffix = unique_suffix();
    let (tx, cleanup) = begin_probe(session, &suffix).await?;
    let shareable = matches!(cleanup, Cleanup::OwnedTransaction);

    let result = probe_inner(session, sql, &suffix, &tx).await;

    // Clean up whatever happened, and let the probe's own result stand.
    if let Err(e) = end_probe(session, &tx, cleanup).await {
        tracing::warn!("could not clean up after a describe probe: {}", e.message);
    }
    result.map(|shape| (shape, shareable))
}

/// Get a transaction to probe in, without endangering the caller's.
async fn begin_probe(session: &Session, suffix: &str) -> Result<(String, Cleanup), ErrorInfo> {
    match session.transaction_id().await {
        Some(tx) => {
            let name = format!("dapi_probe_{suffix}");
            session
                .execute_in(&format!("SAVEPOINT {name}"), vec![], Some(&tx))
                .await?;
            Ok((tx, Cleanup::Savepoint(name)))
        }
        None => {
            let tx = session.api().begin_transaction().await?;
            Ok((tx, Cleanup::OwnedTransaction))
        }
    }
}

async fn end_probe(session: &Session, tx: &str, cleanup: Cleanup) -> Result<(), ErrorInfo> {
    match cleanup {
        Cleanup::OwnedTransaction => session.api().rollback_transaction(tx).await,
        Cleanup::Savepoint(name) => {
            // Rewinding undoes the PREPARE and clears any error the probe left
            // the transaction in.
            session
                .execute_in(&format!("ROLLBACK TO SAVEPOINT {name}"), vec![], Some(tx))
                .await?;
            session
                .execute_in(&format!("RELEASE SAVEPOINT {name}"), vec![], Some(tx))
                .await
                .map(|_| ())
        }
    }
}

async fn probe_inner(
    session: &Session,
    sql: &str,
    suffix: &str,
    tx: &str,
) -> Result<StatementShape, ErrorInfo> {
    let stmt_name = format!("dapi_ps_{suffix}");

    // `$n` placeholders inside a PREPARE body belong to the prepared statement,
    // so the Data API sees a statement with no parameters of its own and passes
    // the text through untouched.
    session
        .execute_in(&format!("PREPARE {stmt_name} AS {sql}"), vec![], Some(tx))
        .await?;

    let (param_types, result_oids) = read_prepared_metadata(session, &stmt_name, tx).await?;

    let fields = if result_oids.is_empty() {
        Vec::new()
    } else if sql::modifies_data(sql) {
        // A data-modifying statement cannot be wrapped in a subquery, and
        // wrapping it in a CTE to get around that performs the write.
        returning_fields(session, sql, &result_oids, tx).await?
    } else {
        select_fields(session, sql, &param_types, Some(tx)).await?
    };

    Ok(StatementShape {
        param_types,
        fields,
    })
}

/// Read the parameter and result types PostgreSQL inferred for the statement.
///
/// Parameter types come back as names, because those go straight into the
/// `CAST(:pN AS ...)` the proxy writes, and a name covers types the proxy has
/// no OID mapping for, such as an enum. Result types come back as OIDs, which
/// is what building a `RowDescription` needs.
async fn read_prepared_metadata(
    session: &Session,
    stmt_name: &str,
    tx: &str,
) -> Result<(Vec<Option<String>>, Vec<i64>), ErrorInfo> {
    // `regtype[]` is a type the Data API refuses to return, so both arrays are
    // converted before they leave the server.
    let query = format!(
        "SELECT \
           (SELECT array_agg(t::text ORDER BY ord) \
              FROM unnest(parameter_types) WITH ORDINALITY u(t, ord)) AS pnames, \
           (SELECT array_agg(t::oid::int8 ORDER BY ord) \
              FROM unnest(result_types) WITH ORDINALITY u(t, ord)) AS roids \
         FROM pg_prepared_statements WHERE name = '{stmt_name}'"
    );

    let outcome = session.execute_in(&query, vec![], Some(tx)).await?;
    let Outcome::Rows { records, .. } = outcome else {
        return Err(internal("pg_prepared_statements returned no result set"));
    };
    let Some(row) = records.first() else {
        return Err(internal(
            "the prepared statement vanished before it could be described",
        ));
    };

    let param_types = string_array(row.first()).into_iter().collect();
    let result_oids = long_array(row.get(1));
    Ok((param_types, result_oids))
}

fn string_array(field: Option<&aws_sdk_rdsdata::types::Field>) -> Vec<Option<String>> {
    use aws_sdk_rdsdata::types::{ArrayValue, Field};
    match field {
        Some(Field::ArrayValue(ArrayValue::StringValues(v))) => v.clone(),
        _ => Vec::new(),
    }
}

fn long_array(field: Option<&aws_sdk_rdsdata::types::Field>) -> Vec<i64> {
    use aws_sdk_rdsdata::types::{ArrayValue, Field};
    match field {
        Some(Field::ArrayValue(ArrayValue::LongValues(v))) => v.iter().filter_map(|x| *x).collect(),
        _ => Vec::new(),
    }
}

/// Recover the result columns of a read-only statement, without running it.
///
/// The parameters become typed NULLs, and the whole thing is wrapped in
/// `SELECT * FROM (...) WHERE false`. PostgreSQL plans that as a one-time false
/// filter, so the inner query is never executed, and the Data API returns the
/// column metadata in full even though no rows come back.
///
/// `CREATE TEMP TABLE ... AS EXECUTE ... WITH NO DATA` would also work and was
/// the first approach here, but it fails outright on a result with two columns
/// of the same name -- `SELECT a.id, b.id FROM ...` -- with
/// `column "id" specified more than once`. A subquery has no such trouble.
async fn select_fields(
    session: &Session,
    sql: &str,
    param_types: &[Option<String>],
    tx: Option<&str>,
) -> Result<Vec<FieldInfo>, ErrorInfo> {
    let substituted = sql::substitute_null_params(sql, param_types);
    let outcome = session
        .execute_in(
            &format!("SELECT * FROM ({substituted}) AS dapi_shape WHERE false"),
            vec![],
            tx,
        )
        .await?;
    let Outcome::Rows { columns, .. } = outcome else {
        return Err(internal("the shape query returned no column metadata"));
    };

    // `Describe(statement)` always reports text format; the client picks the
    // real format later, at Bind.
    Ok(exec::build_fields(&columns, &Format::UnifiedText))
}

/// Recover the result columns of a `RETURNING` clause.
///
/// Types are authoritative, from the server. Names are read off the clause,
/// which is exact for every form a client actually sends; where the clause
/// cannot name a column, PostgreSQL's own `?column?` is used, and if the names
/// do not line up with the types at all the proxy falls back to positional
/// names rather than pairing a name with the wrong column.
async fn returning_fields(
    session: &Session,
    sql: &str,
    result_oids: &[i64],
    tx: &str,
) -> Result<Vec<FieldInfo>, ErrorInfo> {
    let types = exec::types_from_oids(result_oids);
    let mut names = match sql::returning_items(sql) {
        Some(items) => expand_returning(session, sql, &items, tx).await,
        // A data-modifying CTE: the columns come from the outer select list,
        // which nothing here can see without running the write.
        None => Vec::new(),
    };

    if names.len() != types.len() {
        if !names.is_empty() {
            tracing::debug!(
                "RETURNING gave {} names for {} columns; using positional names",
                names.len(),
                types.len()
            );
        }
        names = (1..=types.len()).map(|i| format!("column{i}")).collect();
    }

    Ok(names
        .into_iter()
        .zip(types)
        .map(|(name, ty)| {
            FieldInfo::new(name, None, None, ty.clone(), FieldFormat::Text)
                .with_type_size(types::type_size(&ty))
        })
        .collect())
}

/// Turn a parsed `RETURNING` list into concrete column names.
async fn expand_returning(
    session: &Session,
    sql: &str,
    items: &[ReturningItem],
    tx: &str,
) -> Vec<String> {
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        match item {
            ReturningItem::Named(n) => names.push(n.clone()),
            ReturningItem::Unnamed => names.push("?column?".to_string()),
            ReturningItem::Star => match star_columns(session, sql, tx).await {
                Some(cols) => names.extend(cols),
                // Without the table's columns there is no way to line names up
                // with types; an empty list triggers the positional fallback.
                None => return Vec::new(),
            },
        }
    }
    names
}

/// The columns of the table a data-modifying statement writes to, in order.
async fn star_columns(session: &Session, sql: &str, tx: &str) -> Option<Vec<String>> {
    let table = sql::dml_target_table(sql)?;
    let outcome = session
        .execute_in(
            &format!("SELECT * FROM {table} WHERE false"),
            vec![],
            Some(tx),
        )
        .await
        .ok()?;
    match outcome {
        Outcome::Rows { columns, .. } => Some(
            columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    c.label
                        .clone()
                        .or_else(|| c.name.clone())
                        .unwrap_or_else(|| format!("column{}", i + 1))
                })
                .collect(),
        ),
        Outcome::Affected { .. } => None,
    }
}

/// The parameter types to use for a statement, preferring what the client said.
///
/// A client may declare some, all or none of its parameter types in `Parse`.
/// Whatever it leaves out, the probe fills in.
pub fn merge_param_types(
    client_types: &[Option<Type>],
    probed: &[Option<String>],
    count: usize,
) -> Vec<Option<String>> {
    (0..count)
        .map(|i| {
            client_types
                .get(i)
                .and_then(|t| t.clone())
                .and_then(|t| crate::params::cast_name_for(&t))
                .or_else(|| probed.get(i).cloned().flatten())
        })
        .collect()
}

fn internal(message: &str) -> ErrorInfo {
    ErrorInfo::new(
        "ERROR".to_string(),
        "XX000".to_string(),
        message.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_client_declared_parameter_types() {
        let client = vec![Some(Type::INT4), None];
        let probed = vec![Some("text".to_string()), Some("numeric".to_string())];
        assert_eq!(
            merge_param_types(&client, &probed, 2),
            vec![Some("int4".to_string()), Some("numeric".to_string())]
        );
    }

    #[test]
    fn falls_back_to_probed_types_when_the_client_says_nothing() {
        assert_eq!(
            merge_param_types(&[], &[Some("int4".to_string())], 1),
            vec![Some("int4".to_string())]
        );
    }

    #[test]
    fn leaves_a_parameter_untyped_when_neither_side_knows() {
        assert_eq!(merge_param_types(&[], &[], 2), vec![None, None]);
    }

    #[test]
    fn ignores_an_unknown_type_the_client_sent() {
        // OID 0 means "you decide", so the probe's answer wins.
        let client = vec![Some(Type::UNKNOWN)];
        let probed = vec![Some("int4".to_string())];
        assert_eq!(
            merge_param_types(&client, &probed, 1),
            vec![Some("int4".to_string())]
        );
    }

    #[test]
    fn probe_names_are_unique() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b);
    }

    #[test]
    fn one_call_answers_a_read_with_no_placeholders() {
        assert!(shape_query_stands_alone("SELECT 1"));
        assert!(shape_query_stands_alone("select a, b from t where c = 'x'"));
        assert!(shape_query_stands_alone(
            "WITH x AS (SELECT 1) SELECT * FROM x"
        ));
        assert!(shape_query_stands_alone("VALUES (1, 'a')"));
        assert!(shape_query_stands_alone("TABLE t"));
        assert!(shape_query_stands_alone("  -- a comment\n  SELECT 1"));
        // A literal that looks like a placeholder is not one.
        assert!(shape_query_stands_alone("SELECT '$1'"));
    }

    #[test]
    fn a_placeholder_forces_the_long_way() {
        // The wrap needs a typed NULL here, and the type comes from PREPARE.
        assert!(!shape_query_stands_alone("SELECT $1"));
        assert!(!shape_query_stands_alone("select * from t where a = $1"));
        assert!(!shape_query_stands_alone("select $2, $1"));
    }

    #[test]
    fn a_write_forces_the_long_way() {
        // A write cannot be wrapped in a subquery, and wrapping it in a
        // data-modifying CTE performs the write.
        assert!(!shape_query_stands_alone("INSERT INTO t VALUES (1)"));
        assert!(!shape_query_stands_alone("UPDATE t SET a = 1"));
        assert!(!shape_query_stands_alone("DELETE FROM t"));
        assert!(!shape_query_stands_alone(
            "WITH x AS (INSERT INTO t VALUES (1) RETURNING id) SELECT * FROM x"
        ));
        // `FOR UPDATE` takes locks, so the wrap is not free of consequences.
        assert!(!shape_query_stands_alone("select * from t for update"));
    }

    #[test]
    fn a_statement_a_subquery_cannot_hold_forces_the_long_way() {
        assert!(!shape_query_stands_alone("SHOW server_version"));
        assert!(!shape_query_stands_alone("EXPLAIN SELECT 1"));
        assert!(!shape_query_stands_alone("SET search_path = public"));
        assert!(!shape_query_stands_alone("CREATE TABLE t (a int)"));
        assert!(!shape_query_stands_alone("BEGIN"));
        assert!(!shape_query_stands_alone(""));
    }

    /// Inside a transaction the short way is refused whatever the statement is.
    ///
    /// A statement that does not compile aborts a PostgreSQL transaction, and
    /// describing a statement that does not compile is an ordinary thing for a
    /// client to do. The long way runs behind a savepoint for that reason; the
    /// short way has no savepoint to hide behind.
    #[tokio::test]
    async fn an_open_transaction_refuses_the_short_way() {
        let session = offline_session();
        assert!(
            one_call_is_enough(&session, "SELECT 1").await,
            "with no transaction open there is nothing to protect"
        );

        session.adopt_transaction("tx-1".to_string()).await;
        assert!(
            !one_call_is_enough(&session, "SELECT 1").await,
            "the caller's transaction must not be put at risk to save four calls"
        );
    }

    /// A session with no credentials and nothing to talk to.
    fn offline_session() -> Session {
        use crate::dataapi::DataApi;
        use crate::session::SharedShapes;

        let conf = aws_sdk_rdsdata::Config::builder()
            .behavior_version(aws_sdk_rdsdata::config::BehaviorVersion::latest())
            .region(aws_sdk_rdsdata::config::Region::new("us-east-1"))
            .build();
        let api = DataApi::new(
            aws_sdk_rdsdata::Client::from_conf(conf),
            "arn:aws:rds:us-east-1:1:cluster:c".into(),
            "arn:aws:secretsmanager:us-east-1:1:secret:s".into(),
            "postgres".into(),
            std::time::Duration::from_secs(1),
        );
        Session::new(api, Arc::new(SharedShapes::shared()))
    }
}
