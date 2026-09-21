//! Command line and environment configuration.

use clap::{Parser, ValueEnum};

/// How widely the proxy reuses what it has worked out about a statement.
///
/// Answering `Describe` costs five Data API calls, and the answer is a
/// property of the schema rather than of the connection that asked.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeCache {
    /// Share between every connection in this process.
    Process,
    /// Keep each connection's own, as if no other connection existed.
    Connection,
}

/// Speak the PostgreSQL wire protocol on a local socket and forward queries to
/// an Aurora cluster over the RDS Data API.
#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Config {
    /// ARN of the Aurora cluster to query.
    #[arg(long, env = "CLUSTER_ARN")]
    pub cluster_arn: String,

    /// ARN of the Secrets Manager secret holding the database credentials.
    #[arg(long, env = "SECRET_ARN")]
    pub secret_arn: String,

    /// Database to connect to.
    #[arg(long, env = "DATABASE")]
    pub database: String,

    /// Address to listen on.
    ///
    /// This defaults to loopback on purpose. The proxy accepts any password,
    /// because the real credentials are the AWS ones it resolves for itself, so
    /// binding it to a routable address would hand the cluster to whoever can
    /// reach the port.
    #[arg(long, env = "LISTEN", default_value = "127.0.0.1:5432")]
    pub listen: String,

    /// AWS region, if it should differ from the one the credential chain finds.
    #[arg(long, env = "AWS_REGION")]
    pub region: Option<String>,

    /// How long to keep retrying while a scaled-to-zero cluster resumes.
    #[arg(long, env = "RESUME_TIMEOUT_SECS", default_value_t = 90)]
    pub resume_timeout_secs: u64,

    /// Log level: error, warn, info, debug or trace.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// How widely to reuse what the proxy learns about a statement's shape.
    ///
    /// Answering a client's `Describe` costs five Data API calls, and the
    /// answer belongs to the schema rather than to the connection that asked,
    /// so by default every connection in this process can use it. That is what
    /// keeps a pool that opens a connection per request, or a Lambda whose
    /// handler reconnects, from probing the same statement over and over.
    ///
    /// The proxy empties the cache when it sees DDL go past. It cannot see DDL
    /// run by anything else, so if migrations reach the cluster by another
    /// route while this proxy is running, `connection` narrows the window to
    /// one connection's lifetime.
    #[arg(long, env = "DESCRIBE_CACHE", value_enum, default_value_t = DescribeCache::Process)]
    pub describe_cache: DescribeCache,
}

impl Config {
    /// Whether the listen address is loopback.
    ///
    /// Used to warn loudly when it is not: the proxy accepts any password from
    /// its clients.
    pub fn listens_on_loopback(&self) -> bool {
        use std::net::{IpAddr, ToSocketAddrs};
        match self.listen.to_socket_addrs() {
            Ok(addrs) => {
                let mut saw_one = false;
                for addr in addrs {
                    saw_one = true;
                    let is_loopback = match addr.ip() {
                        IpAddr::V4(v4) => v4.is_loopback(),
                        IpAddr::V6(v6) => v6.is_loopback(),
                    };
                    if !is_loopback {
                        return false;
                    }
                }
                saw_one
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(listen: &str) -> Config {
        Config {
            cluster_arn: "arn:aws:rds:us-east-1:1:cluster:c".into(),
            secret_arn: "arn:aws:secretsmanager:us-east-1:1:secret:s".into(),
            database: "postgres".into(),
            listen: listen.into(),
            region: None,
            resume_timeout_secs: 90,
            log_level: "info".into(),
            describe_cache: DescribeCache::Process,
        }
    }

    #[test]
    fn recognises_loopback_addresses() {
        assert!(config("127.0.0.1:5432").listens_on_loopback());
        assert!(config("[::1]:5432").listens_on_loopback());
        assert!(config("localhost:5432").listens_on_loopback());
    }

    #[test]
    fn recognises_non_loopback_addresses() {
        assert!(!config("0.0.0.0:5432").listens_on_loopback());
        assert!(!config("[::]:5432").listens_on_loopback());
    }
}
