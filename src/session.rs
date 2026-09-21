//! Per-connection state.
//!
//! The Data API has no connections, so everything a PostgreSQL session is
//! expected to remember lives here instead: which Data API transaction is open,
//! what the proxy has already learned about each prepared statement, and the
//! results it executed early in order to answer a `Describe`.
//!
//! One `Session` is held in pgwire's per-connection extension store, so it is
//! dropped exactly when the client goes away -- which is also when an
//! unfinished transaction has to be rolled back.

use std::collections::HashMap;
use std::sync::Arc;

use pgwire::api::portal::PortalExecutionState;
use pgwire::api::results::{FieldInfo, Tag};
use pgwire::error::ErrorInfo;
use tokio::sync::Mutex;

use crate::dataapi::{DataApi, Outcome};

/// What the proxy has worked out about a prepared statement.
#[derive(Debug, Clone)]
pub struct StatementShape {
    /// SQL type names for each parameter, in order, for the casts that make
    /// binding work. A `None` means the type could not be determined.
    pub param_types: Vec<Option<String>>,
    /// The result columns. Empty means the statement returns no rows.
    pub fields: Vec<FieldInfo>,
}

/// A result the proxy produced while answering `Describe`, waiting for the
/// `Execute` that follows.
///
/// Rows are handed to pgwire's own portal state instead; this covers the
/// statements that report only a count, whose command tag has no other place
/// to live.
struct Pending {
    /// Keeps the portal's state alive, so its address cannot be reused by a
    /// different portal while this entry is in the map.
    _keepalive: Arc<Mutex<PortalExecutionState>>,
    tag: Tag,
}

/// How many statement shapes one cache holds before starting over.
const SHAPE_CACHE_LIMIT: usize = 512;

/// Statement shapes every connection in this process can use.
///
/// A shape is a property of the schema rather than of a connection, so the
/// five Data API calls that work one out need not be paid again by the next
/// connection to ask. That matters most where connections are short: a pool
/// that opens one per request, or a Lambda whose handler reconnects, otherwise
/// probes every statement every time.
///
/// It is deliberately empty when constructed and deliberately easy to clear.
/// See [`Session::forget_shapes`] for when it is.
#[derive(Debug, Default)]
pub struct SharedShapes {
    entries: Mutex<HashMap<String, Arc<StatementShape>>>,
    /// Off means every `insert` is dropped on the floor, which is what
    /// `--describe-cache connection` asks for.
    shared: bool,
}

impl SharedShapes {
    /// A store that connections share.
    pub fn shared() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            shared: true,
        }
    }

    /// A store that never holds anything, leaving each connection its own.
    pub fn disabled() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            shared: false,
        }
    }

    async fn get(&self, sql: &str) -> Option<Arc<StatementShape>> {
        if !self.shared {
            return None;
        }
        self.entries.lock().await.get(sql).cloned()
    }

    async fn insert(&self, sql: String, shape: Arc<StatementShape>) {
        if !self.shared {
            return;
        }
        let mut entries = self.entries.lock().await;
        if entries.len() >= SHAPE_CACHE_LIMIT {
            entries.clear();
        }
        entries.insert(sql, shape);
    }

    async fn clear(&self) {
        self.entries.lock().await.clear();
    }

    /// How many shapes are held. For tests.
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Everything one client connection remembers.
pub struct Session {
    api: DataApi,
    transaction: Mutex<Option<String>>,
    /// True once a statement has failed inside the open transaction. PostgreSQL
    /// rejects everything but a rollback in that state, and so must we.
    failed: Mutex<bool>,
    describe_cache: Mutex<HashMap<String, Arc<StatementShape>>>,
    /// Shapes this connection may take from and add to, shared with the rest
    /// of the process.
    shapes: Arc<SharedShapes>,
    pending: Mutex<HashMap<usize, Pending>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    pub fn new(api: DataApi, shapes: Arc<SharedShapes>) -> Self {
        Self {
            api,
            transaction: Mutex::new(None),
            failed: Mutex::new(false),
            describe_cache: Mutex::new(HashMap::new()),
            shapes,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn api(&self) -> &DataApi {
        &self.api
    }

    /// The open Data API transaction, if any.
    pub async fn transaction_id(&self) -> Option<String> {
        self.transaction.lock().await.clone()
    }

    pub async fn in_transaction(&self) -> bool {
        self.transaction.lock().await.is_some()
    }

    /// Whether the open transaction has already failed.
    pub async fn is_failed(&self) -> bool {
        *self.failed.lock().await
    }

    /// Mark the open transaction as failed, if there is one.
    pub async fn mark_failed(&self) {
        if self.transaction.lock().await.is_some() {
            *self.failed.lock().await = true;
        }
    }

    /// Run a statement in the session's current transaction, if any.
    pub async fn execute(
        &self,
        sql: &str,
        params: Vec<aws_sdk_rdsdata::types::SqlParameter>,
    ) -> Result<Outcome, ErrorInfo> {
        let tx = self.transaction_id().await;
        let result = self.api.execute(sql, params, tx.as_deref()).await;
        if result.is_err() {
            self.mark_failed().await;
        }
        result
    }

    /// Run a statement in an explicitly named transaction, bypassing the
    /// session's own. Used by the `Describe` probe, which manages its own.
    pub async fn execute_in(
        &self,
        sql: &str,
        params: Vec<aws_sdk_rdsdata::types::SqlParameter>,
        transaction_id: Option<&str>,
    ) -> Result<Outcome, ErrorInfo> {
        self.api.execute(sql, params, transaction_id).await
    }

    /// Open a transaction for this session.
    pub async fn begin(&self) -> Result<(), ErrorInfo> {
        let mut slot = self.transaction.lock().await;
        if slot.is_some() {
            // PostgreSQL warns and carries on rather than failing.
            return Ok(());
        }
        *slot = Some(self.api.begin_transaction().await?);
        *self.failed.lock().await = false;
        Ok(())
    }

    /// Commit the open transaction, if any.
    pub async fn commit(&self) -> Result<(), ErrorInfo> {
        let taken = self.transaction.lock().await.take();
        *self.failed.lock().await = false;
        match taken {
            Some(id) => self.api.commit_transaction(&id).await,
            None => Ok(()),
        }
    }

    /// Roll the open transaction back, if any.
    pub async fn rollback(&self) -> Result<(), ErrorInfo> {
        let taken = self.transaction.lock().await.take();
        *self.failed.lock().await = false;
        match taken {
            Some(id) => self.api.rollback_transaction(&id).await,
            None => Ok(()),
        }
    }

    /// Adopt a transaction that was opened outside the session, so that the
    /// session owns it from now on.
    pub async fn adopt_transaction(&self, id: String) {
        *self.transaction.lock().await = Some(id);
    }

    // -- the statement shape cache ------------------------------------------

    pub async fn cached_shape(&self, sql: &str) -> Option<Arc<StatementShape>> {
        if let Some(shape) = self.describe_cache.lock().await.get(sql).cloned() {
            return Some(shape);
        }
        self.shapes.get(sql).await
    }

    /// Remember what a probe worked out.
    ///
    /// `shareable` says whether the answer belongs to the schema or only to
    /// this connection. A probe that ran inside the caller's own transaction
    /// could have seen a `SET LOCAL search_path`, a temporary table or
    /// uncommitted DDL, none of which another connection can see, so those
    /// answers stay here. A probe that opened its own transaction saw the
    /// database as everyone else sees it, and is worth sharing.
    pub async fn cache_shape(&self, sql: String, shape: Arc<StatementShape>, shareable: bool) {
        if shareable {
            self.shapes.insert(sql.clone(), shape.clone()).await;
        }
        // A connection that prepares an unbounded number of distinct
        // statements should not grow this without limit.
        let mut cache = self.describe_cache.lock().await;
        if cache.len() >= SHAPE_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(sql, shape);
    }

    /// Throw away every remembered shape, here and process-wide.
    ///
    /// Called when a statement goes through that could have changed what a
    /// shape would be. It is cheap -- the next `Describe` of each statement
    /// probes again -- and the alternative is describing a statement to a
    /// client in terms that no longer match what it will receive.
    pub async fn forget_shapes(&self) {
        self.describe_cache.lock().await.clear();
        self.shapes.clear().await;
    }

    // -- results held between Describe and Execute --------------------------

    /// Remember the command tag of a statement executed to answer `Describe`.
    ///
    /// Entries are keyed by the address of the portal's execution state, and
    /// hold a reference to it. Holding it is what makes the key sound: while an
    /// entry exists its address cannot be reused by a different portal.
    pub async fn hold_tag(&self, state: Arc<Mutex<PortalExecutionState>>, tag: Tag) {
        let mut pending = self.pending.lock().await;
        // A client that describes a portal and never executes it would
        // otherwise leave the entry behind. Once the portal itself is gone this
        // map holds the only reference, which is the signal to drop it.
        pending.retain(|_, held| Arc::strong_count(&held._keepalive) > 1);
        pending.insert(
            Arc::as_ptr(&state) as usize,
            Pending {
                _keepalive: state,
                tag,
            },
        );
    }

    /// Take back a tag held for a portal, if one is waiting.
    pub async fn take_tag(&self, state: &Arc<Mutex<PortalExecutionState>>) -> Option<Tag> {
        let key = Arc::as_ptr(state) as usize;
        self.pending.lock().await.remove(&key).map(|p| p.tag)
    }
}

impl Drop for Session {
    /// Roll back an unfinished transaction when the client disconnects.
    ///
    /// Without this the transaction would sit on the cluster holding its locks
    /// until the Data API expires it, three minutes later.
    fn drop(&mut self) {
        let Some(id) = self.transaction.get_mut().take() else {
            return;
        };
        let api = self.api.clone();
        // The connection task is dropped inside the runtime, so there is one to
        // spawn on; if there is not, the transaction expires on its own.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = api.rollback_transaction(&id).await {
                    tracing::warn!("could not roll back {id} after disconnect: {}", e.message);
                } else {
                    tracing::debug!("rolled back {id} after disconnect");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shape_records_parameters_and_fields() {
        let shape = StatementShape {
            param_types: vec![Some("int4".into()), None],
            fields: vec![],
        };
        assert_eq!(shape.param_types.len(), 2);
        assert!(shape.fields.is_empty(), "no fields means no rows");
    }

    fn shape(name: &str) -> Arc<StatementShape> {
        Arc::new(StatementShape {
            param_types: vec![Some(name.to_string())],
            fields: vec![],
        })
    }

    #[tokio::test]
    async fn a_shared_store_hands_one_connections_answer_to_the_next() {
        let shapes = SharedShapes::shared();
        shapes.insert("select 1".into(), shape("int4")).await;

        let found = shapes.get("select 1").await.expect("the shape");
        assert_eq!(found.param_types, vec![Some("int4".to_string())]);
    }

    #[tokio::test]
    async fn a_disabled_store_keeps_nothing() {
        let shapes = SharedShapes::disabled();
        shapes.insert("select 1".into(), shape("int4")).await;

        assert!(
            shapes.get("select 1").await.is_none(),
            "--describe-cache connection must not share across connections"
        );
        assert!(shapes.is_empty().await);
    }

    #[tokio::test]
    async fn a_shared_store_starts_over_rather_than_growing_forever() {
        let shapes = SharedShapes::shared();
        for i in 0..=SHAPE_CACHE_LIMIT {
            shapes.insert(format!("select {i}"), shape("int4")).await;
        }
        assert!(
            shapes.len().await <= SHAPE_CACHE_LIMIT,
            "a process that sees unbounded distinct SQL must not grow without limit"
        );
    }

    #[tokio::test]
    async fn clearing_leaves_nothing_behind() {
        let shapes = SharedShapes::shared();
        shapes.insert("select 1".into(), shape("int4")).await;
        shapes.clear().await;
        assert!(shapes.get("select 1").await.is_none());
    }
}
