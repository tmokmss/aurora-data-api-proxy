//! Shared setup for the integration tests.
//!
//! These need a real Aurora cluster with the Data API enabled, and are skipped
//! unless `CLUSTER_ARN`, `SECRET_ARN` and `DATABASE` are all set. Everything
//! that can be checked without a cluster is a unit test instead.

use std::sync::Arc;
use std::time::Duration;

use aurora_data_api_proxy::dataapi::DataApi;
use aurora_data_api_proxy::handlers::ProxyFactory;
use tokio::net::TcpListener;

/// The cluster settings, or `None` when the tests should be skipped.
pub struct Target {
    pub cluster_arn: String,
    pub secret_arn: String,
    pub database: String,
}

impl Target {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            cluster_arn: std::env::var("CLUSTER_ARN").ok()?,
            secret_arn: std::env::var("SECRET_ARN").ok()?,
            database: std::env::var("DATABASE").ok()?,
        })
    }
}

/// Skip the test body unless a cluster is configured.
///
/// A skipped test passes; there is no stable way for a Rust test to report
/// "ignored" at run time, so it says so on stdout instead.
#[macro_export]
macro_rules! require_cluster {
    () => {
        match $crate::common::Target::from_env() {
            Some(target) => target,
            None => {
                eprintln!("skipping: set CLUSTER_ARN, SECRET_ARN and DATABASE to run this");
                return;
            }
        }
    };
}

/// Start a proxy on an ephemeral port and return its connection string.
///
/// The proxy runs in this process, on the test's own runtime, so a test
/// exercises the same code path a real client would without needing the binary
/// on disk.
pub async fn start_proxy(target: &Target) -> String {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Ok(region) = std::env::var("AWS_REGION") {
        loader = loader.region(aws_config::Region::new(region));
    }
    let sdk_config = loader.load().await;

    let api = DataApi::new(
        aws_sdk_rdsdata::Client::new(&sdk_config),
        target.cluster_arn.clone(),
        target.secret_arn.clone(),
        target.database.clone(),
        // A cluster scaled to zero can take a while to wake; the first test to
        // touch it should wait rather than fail.
        Duration::from_secs(120),
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local address").port();

    let factory = Arc::new(ProxyFactory::new(api, "17.0".to_string()));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                continue;
            };
            let factory = factory.clone();
            tokio::spawn(async move {
                let _ = pgwire::tokio::process_socket(socket, None, factory).await;
            });
        }
    });

    format!(
        "host=127.0.0.1 port={port} user=test dbname={}",
        target.database
    )
}

/// Connect a `tokio_postgres` client to a freshly started proxy.
pub async fn connect(target: &Target) -> tokio_postgres::Client {
    let conn_str = start_proxy(target).await;
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
        .await
        .expect("connect to the proxy");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// A table name unique to one test, so tests can run concurrently.
pub fn unique_table(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}
