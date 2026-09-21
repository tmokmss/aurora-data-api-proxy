//! `aurora-data-api-proxy`: speak PostgreSQL locally, talk to Aurora over HTTP.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aurora_data_api_proxy::config::Config;
use aurora_data_api_proxy::dataapi::{DataApi, Outcome};
use aurora_data_api_proxy::handlers::ProxyFactory;
use clap::Parser;
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| config.log_level.clone().into()),
        )
        .init();

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = config.region.clone() {
        loader = loader.region(aws_config::Region::new(region));
    }
    let sdk_config = loader.load().await;
    if sdk_config.region().is_none() {
        anyhow::bail!(
            "no AWS region configured; set AWS_REGION, pass --region, or configure a profile"
        );
    }

    let api = DataApi::new(
        aws_sdk_rdsdata::Client::new(&sdk_config),
        config.cluster_arn.clone(),
        config.secret_arn.clone(),
        config.database.clone(),
        Duration::from_secs(config.resume_timeout_secs),
    );

    // One round trip before the socket opens: it proves the ARNs, the secret
    // and the IAM permissions all work, and reports the failure once and
    // clearly rather than to whichever client happens to connect first.
    let server_version = check_connectivity(&api)
        .await
        .context("could not reach the cluster through the Data API")?;
    tracing::info!(
        "connected to {} (PostgreSQL {server_version}) as database {}",
        config.cluster_arn,
        config.database
    );

    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("could not listen on {}", config.listen))?;

    if !config.listens_on_loopback() {
        tracing::warn!(
            "listening on {}, which is not loopback: this proxy accepts ANY password, so \
             anyone who can reach this port can use your AWS credentials to query the cluster",
            config.listen
        );
    }
    tracing::info!("listening on {}", config.listen);

    let factory = Arc::new(ProxyFactory::new(api, server_version));
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("could not accept a connection: {e}");
                continue;
            }
        };
        // Latency here is dominated by HTTP round trips, so no sense adding
        // Nagle delays on top.
        let _ = socket.set_nodelay(true);
        let factory = factory.clone();
        tokio::spawn(async move {
            tracing::debug!("connection from {peer}");
            if let Err(e) = process_socket(socket, None, factory).await {
                tracing::debug!("connection from {peer} ended: {e}");
            }
        });
    }
}

/// Read the cluster's PostgreSQL version, proving the connection works.
async fn check_connectivity(api: &DataApi) -> Result<String> {
    let outcome = api
        .execute("SHOW server_version", vec![], None)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    match outcome {
        Outcome::Rows { records, .. } => {
            let version = records
                .first()
                .and_then(|r| r.first())
                .and_then(|f| match f {
                    aws_sdk_rdsdata::types::Field::StringValue(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown".to_string());
            Ok(version)
        }
        Outcome::Affected { .. } => Ok("unknown".to_string()),
    }
}
