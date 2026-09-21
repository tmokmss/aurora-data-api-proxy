//! The Data API client, and the translation of its failures into PostgreSQL
//! errors.
//!
//! Everything the proxy does eventually becomes an `ExecuteStatement` call.
//! This module owns that call: the retry loop that covers a cluster waking from
//! zero capacity, and the mapping from the service's exceptions back to error
//! codes a PostgreSQL client will recognise.

use std::time::{Duration, Instant};

use aws_sdk_rdsdata::Client;
use aws_sdk_rdsdata::error::ProvideErrorMetadata;
use aws_sdk_rdsdata::types::{ColumnMetadata, Field, SqlParameter};
use pgwire::error::ErrorInfo;

/// A connection to one database on one cluster.
#[derive(Debug, Clone)]
pub struct DataApi {
    client: Client,
    cluster_arn: String,
    secret_arn: String,
    database: String,
    /// How long to keep retrying while the cluster resumes from zero capacity.
    resume_timeout: Duration,
}

/// What a statement produced.
///
/// The Data API returns column metadata only for statements that yield rows,
/// which is what tells a `SELECT` returning nothing apart from an `UPDATE`
/// touching nothing.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// A result set, possibly empty.
    Rows {
        columns: Vec<ColumnMetadata>,
        records: Vec<Vec<Field>>,
    },
    /// A statement that reported only how many rows it changed.
    Affected { count: i64 },
}

impl DataApi {
    pub fn new(
        client: Client,
        cluster_arn: String,
        secret_arn: String,
        database: String,
        resume_timeout: Duration,
    ) -> Self {
        Self {
            client,
            cluster_arn,
            secret_arn,
            database,
            resume_timeout,
        }
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    /// Run one statement, optionally inside a transaction.
    pub async fn execute(
        &self,
        sql: &str,
        params: Vec<SqlParameter>,
        transaction_id: Option<&str>,
    ) -> Result<Outcome, ErrorInfo> {
        let deadline = Instant::now() + self.resume_timeout;
        let mut backoff = Duration::from_millis(500);

        loop {
            let mut req = self
                .client
                .execute_statement()
                .resource_arn(&self.cluster_arn)
                .secret_arn(&self.secret_arn)
                .database(&self.database)
                .include_result_metadata(true)
                .sql(sql);
            if !params.is_empty() {
                req = req.set_parameters(Some(params.clone()));
            }
            if let Some(tx) = transaction_id {
                req = req.transaction_id(tx);
            }

            match req.send().await {
                Ok(out) => {
                    return Ok(match out.column_metadata {
                        Some(columns) => Outcome::Rows {
                            columns,
                            records: out.records.unwrap_or_default().into_iter().collect(),
                        },
                        None => Outcome::Affected {
                            count: out.number_of_records_updated,
                        },
                    });
                }
                Err(err) => {
                    if is_resuming(&err) && Instant::now() + backoff < deadline {
                        tracing::info!(
                            "cluster is resuming from zero capacity; retrying in {:?}",
                            backoff
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(8));
                        continue;
                    }
                    return Err(map_sdk_error(&err, sql));
                }
            }
        }
    }

    /// Open a Data API transaction and return its id.
    ///
    /// A transaction is also the only way to pin a session: statements outside
    /// one may each land on a different pooled backend.
    pub async fn begin_transaction(&self) -> Result<String, ErrorInfo> {
        let deadline = Instant::now() + self.resume_timeout;
        let mut backoff = Duration::from_millis(500);
        loop {
            let res = self
                .client
                .begin_transaction()
                .resource_arn(&self.cluster_arn)
                .secret_arn(&self.secret_arn)
                .database(&self.database)
                .send()
                .await;
            match res {
                Ok(out) => {
                    return out.transaction_id.ok_or_else(|| {
                        internal("the Data API opened a transaction but returned no id")
                    });
                }
                Err(err) => {
                    if is_resuming(&err) && Instant::now() + backoff < deadline {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(8));
                        continue;
                    }
                    return Err(map_sdk_error(&err, "BEGIN"));
                }
            }
        }
    }

    pub async fn commit_transaction(&self, transaction_id: &str) -> Result<(), ErrorInfo> {
        self.client
            .commit_transaction()
            .resource_arn(&self.cluster_arn)
            .secret_arn(&self.secret_arn)
            .transaction_id(transaction_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|err| map_sdk_error(&err, "COMMIT"))
    }

    pub async fn rollback_transaction(&self, transaction_id: &str) -> Result<(), ErrorInfo> {
        self.client
            .rollback_transaction()
            .resource_arn(&self.cluster_arn)
            .secret_arn(&self.secret_arn)
            .transaction_id(transaction_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|err| map_sdk_error(&err, "ROLLBACK"))
    }
}

/// Whether this failure means the cluster is waking from zero capacity.
fn is_resuming<E: ProvideErrorMetadata>(err: &E) -> bool {
    err.code() == Some("DatabaseResumingException")
}

/// Build an internal-error `ErrorInfo`.
fn internal(message: &str) -> ErrorInfo {
    ErrorInfo::new(
        "ERROR".to_string(),
        "XX000".to_string(),
        message.to_string(),
    )
}

/// Translate a Data API failure into a PostgreSQL error.
///
/// A `DatabaseErrorException` carries the server's own error text, which we
/// unpack back into its fields. Everything else is a condition of the Data API
/// itself with no PostgreSQL equivalent, so we choose the closest SQLSTATE and
/// say plainly what happened -- these are the errors a user is most likely to
/// hit and least likely to understand.
pub fn map_sdk_error<E: ProvideErrorMetadata>(err: &E, sql: &str) -> ErrorInfo {
    let code = err.code().unwrap_or("");
    let message = err.message().unwrap_or("the Data API call failed");

    match code {
        "DatabaseErrorException" => parse_db_error(message),

        "UnsupportedResultException" => {
            // Two very different problems share this exception, and the
            // difference matters to whoever has to fix the query.
            if message.contains("size limit") {
                let mut e = ErrorInfo::new(
                    "ERROR".to_string(),
                    // program_limit_exceeded
                    "54000".to_string(),
                    format!("{message} (this is an Aurora Data API limit, not a PostgreSQL one)"),
                );
                e.hint = Some(
                    "the Data API caps a single result at 1 MB; add a LIMIT, select fewer \
                     columns, or page through the rows with OFFSET/LIMIT or a key range"
                        .to_string(),
                );
                e
            } else {
                let mut e = ErrorInfo::new(
                    "ERROR".to_string(),
                    // feature_not_supported
                    "0A000".to_string(),
                    format!("{message} (this is an Aurora Data API limit, not a PostgreSQL one)"),
                );
                e.hint = Some(
                    "the Data API cannot return this type at all; cast the column to text in \
                     the query, for example `SELECT my_interval::text`"
                        .to_string(),
                );
                e
            }
        }

        "ValidationException" if message.contains("Multistatement") => {
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "the Aurora Data API accepts only one statement per call".to_string(),
            );
            e.hint = Some(
                "the proxy splits a multi-statement query itself, so reaching this means a \
                 single statement still looked like several; send them one at a time"
                    .to_string(),
            );
            e
        }

        "StatementTimeoutException" => ErrorInfo::new(
            "ERROR".to_string(),
            // query_canceled
            "57014".to_string(),
            format!("{message} (the Aurora Data API stops a statement after 45 seconds)"),
        ),

        "DatabaseResumingException" => {
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                // cannot_connect_now
                "57P03".to_string(),
                "the Aurora cluster is still resuming from zero capacity".to_string(),
            );
            e.hint = Some(
                "the proxy already retried for as long as --resume-timeout-secs allows; \
                 raise it, or wait for the cluster to finish scaling up"
                    .to_string(),
            );
            e
        }

        "HttpEndpointNotEnabledException" => {
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                // connection_failure
                "08006".to_string(),
                message.to_string(),
            );
            e.hint = Some(
                "enable the Data API on the cluster: \
                 `aws rds modify-db-cluster --db-cluster-identifier <id> --enable-http-endpoint`"
                    .to_string(),
            );
            e
        }

        "DatabaseNotFoundException" => ErrorInfo::new(
            "ERROR".to_string(),
            // invalid_catalog_name
            "3D000".to_string(),
            message.to_string(),
        ),

        "AccessDeniedException" | "ForbiddenException" => {
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                // invalid_authorization_specification
                "28000".to_string(),
                message.to_string(),
            );
            e.hint = Some(
                "this is an AWS authorization failure, not a database one; check that the \
                 caller may use rds-data:ExecuteStatement on the cluster and read the secret"
                    .to_string(),
            );
            e
        }

        "SecretsErrorException" | "InvalidSecretException" => {
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                "28000".to_string(),
                message.to_string(),
            );
            e.hint = Some(
                "check --secret-arn: it must name a Secrets Manager secret holding the \
                 database username and password, and the caller must be allowed to read it"
                    .to_string(),
            );
            e
        }

        "TransactionNotFoundException" => ErrorInfo::new(
            "ERROR".to_string(),
            // no_active_sql_transaction
            "25P01".to_string(),
            format!("{message} (a Data API transaction expires after three minutes idle)"),
        ),

        "DatabaseUnavailableException" | "ServiceUnavailableError" => ErrorInfo::new(
            "ERROR".to_string(),
            "57P03".to_string(),
            message.to_string(),
        ),

        "BadRequestException" => ErrorInfo::new(
            "ERROR".to_string(),
            "42601".to_string(),
            message.to_string(),
        ),

        "InternalFailure" | "InternalServerErrorException" => {
            // The service sends this with no message at all for a result
            // holding an `infinity` timestamp or date, which is otherwise
            // impossible to diagnose. The proxy already retries such a query
            // with those columns cast to text; reaching here means that did
            // not apply or did not help.
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                format!("InternalFailure: {message}"),
            );
            e.hint = Some(
                "the Aurora Data API fails this way when a result contains an infinite \
                 timestamp or date; casting the column to text, as in \
                 `SELECT my_ts::text`, returns the value"
                    .to_string(),
            );
            e
        }

        _ => {
            tracing::warn!(code, message, sql, "unmapped Data API error");
            ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                if code.is_empty() {
                    message.to_string()
                } else {
                    format!("{code}: {message}")
                },
            )
        }
    }
}

/// Unpack a PostgreSQL error that the Data API has flattened into one string.
///
/// The service renders the server's error as
/// `ERROR: <message>; [Detail: ...;] [Hint: ...;] [Position: N;] SQLState: XXXXX`.
/// The message itself can contain `; `, so the trailing fields are peeled off
/// from the right rather than split on.
pub fn parse_db_error(raw: &str) -> ErrorInfo {
    let mut rest = raw.trim();

    let mut sqlstate = None;
    let mut position = None;
    let mut hint = None;
    let mut detail = None;
    let mut where_context = None;

    // Peel the known trailing fields off, right to left.
    while let Some(idx) = rest.rfind("; ") {
        let (head, tail) = rest.split_at(idx);
        let field = tail.trim_start_matches("; ").trim_end_matches(';');
        let matched = if let Some(v) = field.strip_prefix("SQLState: ") {
            sqlstate = Some(v.trim().to_string());
            true
        } else if let Some(v) = field.strip_prefix("Position: ") {
            position = Some(v.trim().to_string());
            true
        } else if let Some(v) = field.strip_prefix("Hint: ") {
            hint = Some(v.trim().to_string());
            true
        } else if let Some(v) = field.strip_prefix("Detail: ") {
            detail = Some(v.trim().to_string());
            true
        } else if let Some(v) = field.strip_prefix("Where: ") {
            where_context = Some(v.trim().to_string());
            true
        } else {
            false
        };
        if !matched {
            break;
        }
        rest = head;
    }

    // What remains opens with the severity.
    let (severity, message) = match rest.split_once(": ") {
        Some((sev, msg)) if is_severity(sev) => (sev.to_string(), msg.to_string()),
        _ => ("ERROR".to_string(), rest.to_string()),
    };

    let mut info = ErrorInfo::new(
        severity,
        // internal_error, when the service gave us no code to pass on
        sqlstate.unwrap_or_else(|| "XX000".to_string()),
        message.trim_end_matches(';').to_string(),
    );
    info.detail = detail;
    info.hint = hint;
    info.position = position;
    info.where_context = where_context;
    info
}

fn is_severity(s: &str) -> bool {
    matches!(
        s,
        "ERROR" | "FATAL" | "PANIC" | "WARNING" | "NOTICE" | "DEBUG" | "INFO" | "LOG"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every string below was captured from the live service.

    #[test]
    fn parses_a_plain_error() {
        let e = parse_db_error(
            "ERROR: relation \"no_such_table_xyz\" does not exist; Position: 15; SQLState: 42P01",
        );
        assert_eq!(e.severity, "ERROR");
        assert_eq!(e.code, "42P01");
        assert_eq!(e.message, "relation \"no_such_table_xyz\" does not exist");
        assert_eq!(e.position.as_deref(), Some("15"));
        assert_eq!(e.hint, None);
    }

    #[test]
    fn parses_an_error_with_a_hint() {
        let e = parse_db_error(
            "ERROR: operator does not exist: integer = text; Hint: No operator matches the \
             given name and argument types. You might need to add explicit type casts.; \
             Position: 36; SQLState: 42883",
        );
        assert_eq!(e.code, "42883");
        // The message itself contains `: `, which must survive.
        assert_eq!(e.message, "operator does not exist: integer = text");
        assert!(e.hint.unwrap().starts_with("No operator matches"));
        assert_eq!(e.position.as_deref(), Some("36"));
    }

    #[test]
    fn parses_an_error_with_no_position() {
        let e = parse_db_error(
            "ERROR: current transaction is aborted, commands ignored until end of \
             transaction block; SQLState: 25P02",
        );
        assert_eq!(e.code, "25P02");
        assert_eq!(
            e.message,
            "current transaction is aborted, commands ignored until end of transaction block"
        );
        assert_eq!(e.position, None);
    }

    #[test]
    fn parses_a_constraint_violation() {
        let e = parse_db_error(
            "ERROR: null value in column \"name\" of relation \"proxy_test\" violates \
             not-null constraint; SQLState: 23502",
        );
        assert_eq!(e.code, "23502");
        assert!(e.message.starts_with("null value in column \"name\""));
    }

    #[test]
    fn parses_a_syntax_error() {
        let e = parse_db_error(
            "ERROR: syntax error at or near \"SELEKT\"; Position: 1; SQLState: 42601",
        );
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "syntax error at or near \"SELEKT\"");
    }

    #[test]
    fn falls_back_when_the_shape_is_unfamiliar() {
        let e = parse_db_error("something went wrong");
        assert_eq!(e.severity, "ERROR");
        assert_eq!(e.code, "XX000");
        assert_eq!(e.message, "something went wrong");
    }

    #[test]
    fn does_not_mistake_a_colon_in_the_message_for_a_severity() {
        let e = parse_db_error("ERROR: operator does not exist: integer = text; SQLState: 42883");
        assert_eq!(e.message, "operator does not exist: integer = text");
    }
}
