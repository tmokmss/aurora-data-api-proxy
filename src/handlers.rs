//! The wire-protocol handlers.
//!
//! Two paths lead here. The simple query protocol is a near-direct translation:
//! split the string into statements and run each one. The extended query
//! protocol is where the work is, because it asks questions -- `Describe` --
//! that a stateless HTTP API has no way to answer.
//!
//! The proxy answers `Describe(portal)` by running the statement early and
//! keeping the result. That is safe only if the `Execute` that follows cannot
//! run it a second time, and pgwire's own portal state machine is what
//! guarantees it: a portal that has been `start`ed is never handed back to
//! `do_query`. For statements that report only a row count there is no result
//! set to park there, so the command tag is held on the session instead and
//! [`ProxyHandler::on_execute`] consumes it before pgwire's default path can
//! run anything.

use std::sync::Arc;

use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::auth::{
    DefaultServerParameterProvider, StartupHandler, finish_authentication, protocol_negotiation,
    save_startup_parameters_to_metadata,
};
use pgwire::api::portal::Portal;
use pgwire::api::query::{
    ExtendedQueryHandler, SimpleQueryHandler, send_describe_response, send_execution_response,
};
use pgwire::api::results::{
    DescribePortalResponse, DescribeResponse, DescribeStatementResponse, Response, Tag,
};
use pgwire::api::stmt::{QueryParser, StoredStatement};
use pgwire::api::store::{Entry, PortalStore};
use pgwire::api::{
    ClientInfo, ClientPortalStore, DEFAULT_NAME, PgWireConnectionState, PgWireServerHandlers,
    PidSecretKeyGenerator, RandomPidSecretKeyGenerator, Type,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::extendedquery::{Execute, TARGET_TYPE_BYTE_PORTAL};
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};

use crate::dataapi::{DataApi, Outcome};
use crate::exec::{self, TxControl};
use crate::params;
use crate::probe;
use crate::session::Session;
use crate::sql;
use crate::types::CastScope;

/// A parsed statement: the SQL exactly as the client wrote it.
///
/// Rewriting is deferred until `Bind`, because it depends on the parameter
/// types, which are not always known at `Parse`.
#[derive(Debug, Clone)]
pub struct ProxyStatement {
    pub sql: String,
}

/// Stores the SQL; the real work happens later.
#[derive(Debug, Default)]
pub struct ProxyQueryParser;

#[async_trait]
impl QueryParser for ProxyQueryParser {
    type Statement = ProxyStatement;

    async fn parse_sql<C>(
        &self,
        _client: &C,
        sql: &str,
        _types: &[Option<Type>],
    ) -> PgWireResult<Option<Self::Statement>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        Ok(Some(ProxyStatement {
            sql: sql.to_string(),
        }))
    }

    fn get_parameter_types(&self, _stmt: &Self::Statement) -> PgWireResult<Vec<Type>> {
        // `do_describe_statement` is overridden, so this is never consulted.
        Ok(vec![])
    }

    fn get_result_schema(
        &self,
        _stmt: &Self::Statement,
        _column_format: Option<&pgwire::api::portal::Format>,
    ) -> PgWireResult<Vec<pgwire::api::results::FieldInfo>> {
        Ok(vec![])
    }
}

/// Serves both query protocols for one cluster.
pub struct ProxyHandler {
    api: DataApi,
    parser: Arc<ProxyQueryParser>,
}

impl ProxyHandler {
    pub fn new(api: DataApi) -> Self {
        Self {
            api,
            parser: Arc::new(ProxyQueryParser),
        }
    }

    /// The per-connection state, created on first use.
    fn session<C: ClientInfo>(&self, client: &C) -> Arc<Session> {
        let api = self.api.clone();
        client
            .session_extensions()
            .get_or_insert_with(move || Session::new(api))
    }

    /// Run one statement and turn the result into a response.
    ///
    /// `format` decides how result columns are encoded; the simple protocol is
    /// always text.
    async fn run_statement(
        &self,
        session: &Session,
        raw_sql: &str,
        params: Vec<aws_sdk_rdsdata::types::SqlParameter>,
        send_sql: &str,
        format: &pgwire::api::portal::Format,
    ) -> Result<Response, ErrorInfo> {
        if let Some(control) = exec::transaction_control(raw_sql) {
            return self.run_transaction_control(session, control).await;
        }
        // The Data API mis-parses a few constructs badly enough to corrupt a
        // value silently, so those are refused rather than forwarded.
        if let Some(hazard) = sql::find_hazard(send_sql) {
            let mut e = ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                format!(
                    "this statement cannot be sent through the Aurora Data API: {}",
                    hazard.message()
                ),
            );
            e.detail = Some(
                "the proxy refuses the statement because the Data API would otherwise \
                 change its meaning without reporting an error"
                    .to_string(),
            );
            return Err(e);
        }

        match self
            .execute_with_fallback(session, send_sql, params)
            .await?
        {
            Outcome::Rows { columns, records } => {
                exec::rows_response(raw_sql, &columns, &records, format)
            }
            Outcome::Affected { count } => {
                Ok(Response::Execution(exec::affected_tag(raw_sql, count)))
            }
        }
    }

    /// Run a statement, retrying once past the Data API's unreturnable types.
    ///
    /// A single `interval` or `"char"` column fails the whole query with
    /// `UnsupportedResultException`, which is what stops psql's `\d` from
    /// working. When that happens the proxy asks for the result's shape -- a
    /// zero-row query returns metadata without tripping the limit -- and runs
    /// the statement again with those columns cast to text. They are columns
    /// the proxy would have delivered as text anyway, so nothing is lost.
    async fn execute_with_fallback(
        &self,
        session: &Session,
        send_sql: &str,
        params: Vec<aws_sdk_rdsdata::types::SqlParameter>,
    ) -> Result<Outcome, ErrorInfo> {
        let tx = session.transaction_id().await;
        let first = session
            .execute_in(send_sql, params.clone(), tx.as_deref())
            .await;

        let (original, scope) = match first {
            Ok(outcome) => return Ok(outcome),
            Err(e) if is_unreturnable_type(&e) => (e, CastScope::Unreturnable),
            // A bare `InternalFailure` is what the Data API returns for an
            // `infinity` timestamp, with no message to say so. Casting the
            // temporal columns to text is the only way to get the rows out.
            Err(e) if is_internal_failure(&e) => (e, CastScope::AlsoTemporal),
            Err(e) => {
                session.mark_failed().await;
                return Err(e);
            }
        };

        match self
            .cast_fallback_sql(session, send_sql, &params, scope)
            .await
        {
            Some(rewritten) => {
                tracing::debug!("retrying with text casts: {rewritten}");
                match session.execute_in(&rewritten, params, tx.as_deref()).await {
                    Ok(outcome) => Ok(outcome),
                    Err(_) => {
                        // Report the problem the user can act on, not the
                        // failure of our own workaround.
                        session.mark_failed().await;
                        Err(original)
                    }
                }
            }
            None => {
                session.mark_failed().await;
                Err(original)
            }
        }
    }

    /// Warn the client about a statement that will succeed and do nothing.
    ///
    /// Outside a transaction, every statement may land on a different pooled
    /// backend session, so anything whose whole purpose is to change session
    /// state is lost the moment it returns. `SET search_path = ...` is the one
    /// that bites: it reports `SET`, and the next query runs with the old path.
    /// Reporting an error would be wrong -- the statement did run -- so the
    /// client gets a notice instead.
    async fn notice_if_ineffective<C>(
        &self,
        client: &mut C,
        session: &Session,
        sql: &str,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let words = sql::leading_words(sql, 2);
        let second = words.get(1).map(String::as_str);
        let warning = match words.first().map(String::as_str) {
            // `SET TRANSACTION` and `SET CONSTRAINTS` are transaction-scoped
            // anyway, so they are not surprising.
            Some("SET") if !matches!(second, Some("TRANSACTION" | "CONSTRAINTS")) => {
                if session.in_transaction().await {
                    return Ok(());
                }
                Some((
                    "this SET will not affect later statements",
                    "the Aurora Data API pins a session only inside a transaction, so \
                     session settings are lost as soon as the statement returns; run it \
                     inside BEGIN/COMMIT, or qualify names explicitly",
                ))
            }
            Some("LISTEN") => Some((
                "LISTEN has no effect through this proxy",
                "the Aurora Data API has no way to deliver asynchronous notifications, \
                 so no NOTIFY will ever reach this connection",
            )),
            _ => None,
        };

        if let Some((message, hint)) = warning {
            let mut info = ErrorInfo::new(
                "WARNING".to_string(),
                // warning
                "01000".to_string(),
                message.to_string(),
            );
            info.hint = Some(hint.to_string());
            client
                .feed(PgWireBackendMessage::NoticeResponse(info.into()))
                .await?;
        }
        Ok(())
    }

    /// The columns of an open SQL cursor, if `name` names one.
    ///
    /// `FETCH 0` returns the metadata without moving the cursor.
    async fn describe_cursor<C: ClientInfo>(
        &self,
        client: &C,
        name: &str,
    ) -> Option<Vec<pgwire::api::results::FieldInfo>> {
        let session = self.session(client);
        // A cursor only exists inside a transaction, so there is no point
        // asking outside one -- and asking would open one needlessly.
        let tx = session.transaction_id().await?;
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        match session
            .execute_in(&format!("FETCH 0 FROM {quoted}"), vec![], Some(&tx))
            .await
        {
            Ok(Outcome::Rows { columns, .. }) => Some(exec::build_fields(
                &columns,
                &pgwire::api::portal::Format::UnifiedText,
            )),
            _ => None,
        }
    }

    /// Build a version of a read-only statement with unreturnable columns cast
    /// to text, or `None` if that cannot be done safely.
    async fn cast_fallback_sql(
        &self,
        session: &Session,
        send_sql: &str,
        params: &[aws_sdk_rdsdata::types::SqlParameter],
        scope: CastScope,
    ) -> Option<String> {
        // Only a query can be wrapped in a subselect, and only a statement
        // that writes nothing may be run a second time.
        if sql::modifies_data(send_sql) {
            return None;
        }
        if !matches!(
            sql::leading_words(send_sql, 1).first().map(String::as_str),
            Some("SELECT" | "WITH" | "TABLE" | "VALUES")
        ) {
            return None;
        }

        let alias = "__dapi_cast";
        let shape = session
            .execute_in(
                &format!("SELECT * FROM ({send_sql}) AS {alias} LIMIT 0"),
                params.to_vec(),
                session.transaction_id().await.as_deref(),
            )
            .await
            .ok()?;
        let Outcome::Rows { columns, .. } = shape else {
            return None;
        };
        if columns.is_empty() {
            return None;
        }

        // Selecting by name from the subquery needs the names to be distinct
        // and present; if they are not, leave the original error alone rather
        // than send something that means a different thing.
        let mut seen = std::collections::HashSet::new();
        let mut list = Vec::with_capacity(columns.len());
        let mut any_cast = false;
        for col in &columns {
            let name = col.label.as_deref().or(col.name.as_deref())?;
            if name.is_empty() || !seen.insert(name.to_string()) {
                return None;
            }
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            match crate::types::needs_text_cast(col.type_name.as_deref().unwrap_or("text"), scope) {
                Some(cast) => {
                    any_cast = true;
                    list.push(format!("{quoted}::{cast} AS {quoted}"));
                }
                None => list.push(quoted),
            }
        }
        if !any_cast {
            return None;
        }
        Some(format!(
            "SELECT {} FROM ({send_sql}) AS {alias}",
            list.join(", ")
        ))
    }

    async fn run_transaction_control(
        &self,
        session: &Session,
        control: TxControl,
    ) -> Result<Response, ErrorInfo> {
        match control {
            TxControl::Begin => {
                session.begin().await?;
                Ok(Response::TransactionStart(Tag::new("BEGIN")))
            }
            TxControl::Commit => {
                // PostgreSQL turns a COMMIT in a failed transaction into a
                // rollback, and reports it as one.
                if session.is_failed().await {
                    session.rollback().await?;
                    return Ok(Response::TransactionEnd(Tag::new("ROLLBACK")));
                }
                session.commit().await?;
                Ok(Response::TransactionEnd(Tag::new("COMMIT")))
            }
            TxControl::Rollback => {
                session.rollback().await?;
                Ok(Response::TransactionEnd(Tag::new("ROLLBACK")))
            }
        }
    }

    /// Work out the SQL and parameters to send for a bound portal.
    async fn prepare_portal(
        &self,
        session: &Session,
        portal: &Portal<ProxyStatement>,
    ) -> Result<(String, Vec<aws_sdk_rdsdata::types::SqlParameter>), ErrorInfo> {
        let sql = &portal.statement.statement.sql;
        let declared = &portal.statement.parameter_types;
        let bound = portal.parameters.len();

        if bound == 0 {
            return Ok((sql::rewrite_placeholders(sql).sql, Vec::new()));
        }

        // A parameter with no declared type has to be cast, or the Data API
        // binds it as `text` and the statement fails on a type mismatch.
        let needs_probe = (0..bound).any(|i| {
            !matches!(declared.get(i).and_then(|t| t.clone()), Some(ref t) if *t != Type::UNKNOWN)
        });
        let probed = if needs_probe {
            match probe::describe_statement(session, sql).await {
                Ok(shape) => shape.param_types.clone(),
                // A failed probe is not fatal: the statement may still work
                // with whatever types the client declared.
                Err(e) => {
                    tracing::debug!(
                        "could not describe {sql:?} for its parameter types: {}",
                        e.message
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let cast_types = probe::merge_param_types(declared, &probed, bound);
        let rewritten = sql::rewrite_placeholders_with_casts(sql, &cast_types);

        let count = bound.min(rewritten.param_count);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            // A client that let the server infer the types -- tokio-postgres
            // and sqlx both do -- encodes its parameters using the types the
            // proxy reported from `Describe`, and never repeats them in `Bind`.
            // So the same answer has to be recovered here to decode them.
            let ty = declared
                .get(i)
                .and_then(|t| t.clone())
                .filter(|t| *t != Type::UNKNOWN)
                .or_else(|| {
                    cast_types
                        .get(i)
                        .and_then(|n| n.as_deref())
                        .map(type_from_sql_name)
                        .filter(|t| *t != Type::UNKNOWN)
                })
                .unwrap_or(Type::UNKNOWN);
            out.push(
                params::to_sql_parameter(
                    i + 1,
                    portal.parameters[i].as_deref(),
                    portal.parameter_format.is_binary(i),
                    &ty,
                )
                .map_err(|e| ErrorInfo::new("ERROR".to_string(), "22P02".to_string(), e.0))?,
            );
        }
        Ok((rewritten.sql, out))
    }
}

#[async_trait]
impl SimpleQueryHandler for ProxyHandler {
    async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.session(client);

        // The Data API takes one statement per call, so a multi-statement
        // query is split and run in order.
        let statements = sql::split_statements(query);
        if statements.is_empty() {
            return Ok(vec![Response::EmptyQuery]);
        }

        let mut responses = Vec::with_capacity(statements.len());
        for stmt in statements {
            self.notice_if_ineffective(client, &session, &stmt).await?;
            let sent = sql::rewrite_placeholders(&stmt).sql;
            match self
                .run_statement(
                    &session,
                    &stmt,
                    Vec::new(),
                    &sent,
                    &pgwire::api::portal::Format::UnifiedText,
                )
                .await
            {
                Ok(response) => responses.push(response),
                Err(e) => {
                    // PostgreSQL abandons the rest of the string after an error.
                    responses.push(Response::Error(Box::new(e)));
                    break;
                }
            }
        }
        Ok(responses)
    }
}

#[async_trait]
impl ExtendedQueryHandler for ProxyHandler {
    type Statement = ProxyStatement;
    type QueryParser = ProxyQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.parser.clone()
    }

    /// Answer `Describe(statement)` without running anything.
    async fn do_describe_statement<C>(
        &self,
        client: &mut C,
        target: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.session(client);
        let sql = &target.statement.sql;

        // Transaction control takes no parameters and returns no rows.
        if exec::transaction_control(sql).is_some() {
            return Ok(DescribeStatementResponse::new(vec![], vec![]));
        }

        let shape = probe::describe_statement(&session, sql)
            .await
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        // The client's own declarations win where it made any.
        let param_count = sql::rewrite_placeholders(sql)
            .param_count
            .max(shape.param_types.len());
        let param_types = (0..param_count)
            .map(|i| {
                target
                    .parameter_types
                    .get(i)
                    .and_then(|t| t.clone())
                    .filter(|t| *t != Type::UNKNOWN)
                    .or_else(|| {
                        shape
                            .param_types
                            .get(i)
                            .and_then(|n| n.as_deref())
                            .map(type_from_sql_name)
                    })
                    .unwrap_or(Type::UNKNOWN)
            })
            .collect::<Vec<_>>();

        Ok(DescribeStatementResponse::new(
            param_types,
            shape.fields.clone(),
        ))
    }

    /// Answer `Describe(portal)` by running the statement and keeping the result.
    ///
    /// The parameters are known by now, so this is the one point where the
    /// proxy can learn the result shape exactly. The result is parked on the
    /// portal, which is what stops `Execute` from running the statement again.
    async fn do_describe_portal<C>(
        &self,
        client: &mut C,
        target: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.session(client);
        let raw_sql = &target.statement.statement.sql;

        // Leave transaction control to `Execute`: beginning a transaction is
        // not something to do while merely answering a question.
        if exec::transaction_control(raw_sql).is_some() {
            return Ok(DescribePortalResponse::no_data());
        }

        let (sent, parameters) = self
            .prepare_portal(&session, target)
            .await
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        let response = self
            .run_statement(
                &session,
                raw_sql,
                parameters,
                &sent,
                &target.result_column_format,
            )
            .await
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        match response {
            Response::Query(query_response) => {
                let fields = query_response.row_schema().as_ref().clone();
                // Parking the rows here is what makes the later `Execute` a
                // replay rather than a second execution.
                target.start(query_response).await;
                Ok(DescribePortalResponse::new(fields))
            }
            Response::Execution(tag) => {
                // No result set to park, so the tag waits on the session and
                // `on_execute` picks it up.
                session.hold_tag(target.state(), tag).await;
                Ok(DescribePortalResponse::no_data())
            }
            Response::Error(e) => Err(PgWireError::UserError(e)),
            _ => Ok(DescribePortalResponse::no_data()),
        }
    }

    /// Run a portal that `Describe` did not already run.
    async fn do_query<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.session(client);
        let raw_sql = portal.statement.statement.sql.clone();

        self.notice_if_ineffective(client, &session, &raw_sql)
            .await?;
        let (sent, parameters) = self
            .prepare_portal(&session, portal)
            .await
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        self.run_statement(
            &session,
            &raw_sql,
            parameters,
            &sent,
            &portal.result_column_format,
        )
        .await
        .map_err(|e| PgWireError::UserError(Box::new(e)))
    }

    /// Describe a SQL-level cursor, which lives outside pgwire's portal store.
    ///
    /// In PostgreSQL, `DECLARE c CURSOR FOR ...` creates a portal named `c` in
    /// the same namespace the protocol's `Describe(portal)` reads, so a client
    /// may describe it without ever having bound it. psycopg's named cursors do
    /// exactly that, and pgwire's store knows nothing about it.
    ///
    /// Asking the server for `FETCH 0` settles it: no rows move, and the Data
    /// API still returns the cursor's column metadata. Cursors matter more here
    /// than on a normal server, because they are the way to read a result
    /// larger than the Data API's 1 MB cap.
    async fn on_describe<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Describe,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if message.target_type == TARGET_TYPE_BYTE_PORTAL {
            let name = message.name.as_deref().unwrap_or(DEFAULT_NAME);
            if client.portal_store().get_portal(name).is_none()
                && let Some(fields) = self.describe_cursor(client, name).await
            {
                return send_describe_response(client, &DescribePortalResponse::new(fields)).await;
            }
        }
        self._on_describe(client, message).await
    }

    /// Consume a result produced while answering `Describe`, if there is one.
    ///
    /// Without this, a statement whose only output is a row count would be run
    /// once for the `Describe` and once more here.
    async fn on_execute<C>(&self, client: &mut C, message: Execute) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let portal_name = message.name.as_deref().unwrap_or(DEFAULT_NAME);
        let held = match client.portal_store().get_portal(portal_name) {
            Some(Entry::Value(portal)) => {
                let session = self.session(client);
                session.take_tag(&portal.state()).await
            }
            _ => None,
        };

        if let Some(tag) = held {
            if !matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
                return Err(PgWireError::NotReadyForQuery);
            }
            send_execution_response(client, tag).await?;
            client.set_state(PgWireConnectionState::ReadyForQuery);
            return Ok(());
        }

        self._on_execute(client, message).await
    }
}

/// Whether this failure is the Data API refusing to return one of the types it
/// cannot represent, as opposed to any other problem.
fn is_unreturnable_type(e: &ErrorInfo) -> bool {
    e.code == "0A000" && e.message.contains("unsupported data type")
}

/// Whether this is the Data API's bare `InternalFailure`, which it returns for
/// an `infinity` timestamp among other things.
fn is_internal_failure(e: &ErrorInfo) -> bool {
    e.code == "XX000" && e.message.contains("InternalFailure")
}

/// Resolve a PostgreSQL type name, as `regtype` renders it, to a `Type`.
///
/// Used only to report parameter types back to a client; a name the proxy does
/// not recognise becomes `unknown`, which tells the client to send text and
/// let the server work it out -- exactly what the `CAST` in the rewritten SQL
/// then does.
fn type_from_sql_name(name: &str) -> Type {
    match name {
        "smallint" | "int2" => Type::INT2,
        "integer" | "int" | "int4" => Type::INT4,
        "bigint" | "int8" => Type::INT8,
        "real" | "float4" => Type::FLOAT4,
        "double precision" | "float8" => Type::FLOAT8,
        "numeric" | "decimal" => Type::NUMERIC,
        "boolean" | "bool" => Type::BOOL,
        "text" => Type::TEXT,
        "character varying" | "varchar" => Type::VARCHAR,
        "character" | "bpchar" => Type::BPCHAR,
        "name" => Type::NAME,
        "oid" => Type::OID,
        "date" => Type::DATE,
        "time without time zone" | "time" => Type::TIME,
        "timestamp without time zone" | "timestamp" => Type::TIMESTAMP,
        "timestamp with time zone" | "timestamptz" => Type::TIMESTAMPTZ,
        "uuid" => Type::UUID,
        "json" => Type::JSON,
        "jsonb" => Type::JSONB,
        "bytea" => Type::BYTEA,
        "integer[]" | "_int4" => Type::INT4_ARRAY,
        "bigint[]" | "_int8" => Type::INT8_ARRAY,
        "smallint[]" | "_int2" => Type::INT2_ARRAY,
        "text[]" | "_text" => Type::TEXT_ARRAY,
        "boolean[]" | "_bool" => Type::BOOL_ARRAY,
        "numeric[]" | "_numeric" => Type::NUMERIC_ARRAY,
        "double precision[]" | "_float8" => Type::FLOAT8_ARRAY,
        "real[]" | "_float4" => Type::FLOAT4_ARRAY,
        "uuid[]" | "_uuid" => Type::UUID_ARRAY,
        // Anything else -- an enum, a domain, a composite -- is reported as
        // `unknown`, which tells the client to send text. The `CAST` the proxy
        // writes into the SQL then names the real type, so it still lands
        // correctly.
        _ => Type::UNKNOWN,
    }
}

/// Accepts every connection, and tells the client what kind of server this is.
pub struct ProxyStartupHandler {
    parameters: DefaultServerParameterProvider,
    pid_generator: RandomPidSecretKeyGenerator,
}

impl ProxyStartupHandler {
    /// `server_version` is the cluster's own, read once at start-up, so that
    /// clients that gate behaviour on the version see the truth.
    pub fn new(server_version: String) -> Self {
        let mut parameters = DefaultServerParameterProvider::default();
        parameters.server_version = server_version;
        // Data API sessions always run in UTC, and the proxy stamps `+00` onto
        // every timestamptz on that basis. Saying so keeps the client's idea of
        // the session zone and the server's in step.
        parameters.time_zone = "UTC".to_string();
        Self {
            parameters,
            pid_generator: RandomPidSecretKeyGenerator::default(),
        }
    }
}

#[async_trait]
impl StartupHandler for ProxyStartupHandler {
    /// No authentication.
    ///
    /// The credentials that matter are the AWS ones the proxy resolves for
    /// itself; a password from the client would secure nothing. This is why the
    /// proxy binds to loopback by default and warns when told not to.
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if let PgWireFrontendMessage::Startup(ref startup) = message {
            protocol_negotiation(client, startup).await?;
            save_startup_parameters_to_metadata(client, startup);
            let (pid, secret_key) = self.pid_generator.generate(client);
            client.set_pid_and_secret_key(pid, secret_key);
            finish_authentication(client, &self.parameters).await?;
        }
        Ok(())
    }
}

/// Ties the handlers together for one listening socket.
pub struct ProxyFactory {
    handler: Arc<ProxyHandler>,
    startup: Arc<ProxyStartupHandler>,
}

impl ProxyFactory {
    pub fn new(api: DataApi, server_version: String) -> Self {
        Self {
            handler: Arc::new(ProxyHandler::new(api)),
            startup: Arc::new(ProxyStartupHandler::new(server_version)),
        }
    }
}

impl PgWireServerHandlers for ProxyFactory {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.startup.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_type_names_postgres_reports() {
        assert_eq!(type_from_sql_name("integer"), Type::INT4);
        assert_eq!(
            type_from_sql_name("timestamp with time zone"),
            Type::TIMESTAMPTZ
        );
        assert_eq!(type_from_sql_name("character varying"), Type::VARCHAR);
        assert_eq!(type_from_sql_name("double precision"), Type::FLOAT8);
        // An enum or other user type: the client is told nothing, and the CAST
        // in the rewritten SQL carries the type instead.
        assert_eq!(type_from_sql_name("my_enum"), Type::UNKNOWN);
    }
}
