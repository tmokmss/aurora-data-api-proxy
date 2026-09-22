//! Measure what the proxy costs, against a real cluster.
//!
//! The question: how much slower is a query through the proxy than the same
//! query sent straight to the Data API? Both halves run in this one process, on
//! one machine, against one cluster, interleaved operation by operation, so the
//! two see the same network conditions rather than two different minutes of
//! them.
//!
//! Round-trip counts are measured too, by an SDK interceptor, so the "why" is
//! observed rather than inferred from the source.
//!
//! ```console
//! $ AWS_PROFILE=... AWS_REGION=us-east-1 \
//!   CLUSTER_ARN=... SECRET_ARN=... DATABASE=postgres \
//!   cargo run --release --example overhead
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aurora_data_api_proxy::dataapi::DataApi;
use aurora_data_api_proxy::handlers::ProxyFactory;
use aws_sdk_rdsdata::Client as RdsClient;
use aws_sdk_rdsdata::config::Intercept;
use aws_sdk_rdsdata::config::interceptors::BeforeSerializationInterceptorContextRef;
use aws_smithy_types::config_bag::ConfigBag;
use tokio::net::TcpListener;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// How many times each variant runs. Enough for a stable median without
/// spending minutes on a paid API.
const ITERS: usize = 40;

/// One row, nothing for the server to do: what is left is protocol.
const TINY: &str = "select 1 as n";

/// A thousand rows, to price the row translation rather than the round trip.
const WIDE: &str = "select g as n, g::text as t from generate_series(1, 1000) g";

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run());
}

// -- counting the Data API calls ---------------------------------------------

/// Counts operations the SDK starts, so a round-trip count is measured.
#[derive(Debug, Clone, Default)]
struct Calls(Arc<AtomicU64>);

impl Calls {
    fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Intercept for Calls {
    fn name(&self) -> &'static str {
        "count-data-api-calls"
    }

    fn read_before_execution(
        &self,
        _ctx: &BeforeSerializationInterceptorContextRef<'_>,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// -- collecting samples ------------------------------------------------------

struct Stat {
    label: &'static str,
    times: Vec<Duration>,
    calls: u64,
}

impl Stat {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            times: Vec::new(),
            calls: 0,
        }
    }

    fn pct(&self, p: f64) -> Duration {
        if self.times.is_empty() {
            return Duration::ZERO;
        }
        let mut v = self.times.clone();
        v.sort_unstable();
        v[((v.len() - 1) as f64 * p).round() as usize]
    }

    fn min(&self) -> Duration {
        self.times.iter().copied().min().unwrap_or(Duration::ZERO)
    }

    fn calls_per_op(&self) -> f64 {
        if self.times.is_empty() {
            0.0
        } else {
            self.calls as f64 / self.times.len() as f64
        }
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

/// The difference between two variants, round by round.
///
/// A single Data API call is 150-200 ms from outside the region, and it varies
/// by more than the proxy costs, so comparing two medians measures the network.
/// The variants are run adjacently on purpose: subtracting each round from its
/// own neighbour cancels the drift that would otherwise drown the answer.
fn paired(slower: &Stat, faster: &Stat) -> (f64, f64) {
    let mut diffs: Vec<f64> = slower
        .times
        .iter()
        .zip(&faster.times)
        .map(|(a, b)| (a.as_secs_f64() - b.as_secs_f64()) * 1000.0)
        .collect();
    diffs.sort_by(f64::total_cmp);
    if diffs.is_empty() {
        return (0.0, 0.0);
    }
    let at = |p: f64| diffs[((diffs.len() - 1) as f64 * p).round() as usize];
    (at(0.5), at(0.9))
}

fn table(title: &str, stats: &[&Stat]) {
    println!("\n{title}");
    println!(
        "  {:<40} {:>3} {:>9} {:>8} {:>8} {:>8}",
        "", "n", "calls/op", "p50 ms", "p90 ms", "min ms"
    );
    for s in stats {
        println!(
            "  {:<40} {:>3} {:>9.2} {:>8} {:>8} {:>8}",
            s.label,
            s.times.len(),
            s.calls_per_op(),
            ms(s.pct(0.5)),
            ms(s.pct(0.9)),
            ms(s.min()),
        );
    }
}

// -- the two ways to ask ------------------------------------------------------

struct Target {
    cluster_arn: String,
    secret_arn: String,
    database: String,
}

async fn direct(client: &RdsClient, t: &Target, sql: &str, metadata: bool) -> usize {
    let out = client
        .execute_statement()
        .resource_arn(&t.cluster_arn)
        .secret_arn(&t.secret_arn)
        .database(&t.database)
        .sql(sql)
        .include_result_metadata(metadata)
        .send()
        .await
        .expect("direct ExecuteStatement");
    out.records().len()
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("set CLUSTER_ARN, SECRET_ARN and DATABASE to a cluster to measure against");
        std::process::exit(2);
    })
}

/// Time one operation and attribute the Data API calls it made to `stat`.
macro_rules! measure {
    ($stat:expr, $calls:expr, $body:expr) => {{
        let before = $calls.get();
        let start = Instant::now();
        let out = $body;
        $stat.times.push(start.elapsed());
        $stat.calls += $calls.get() - before;
        out
    }};
}

async fn run() {
    let target = Target {
        cluster_arn: env("CLUSTER_ARN"),
        secret_arn: env("SECRET_ARN"),
        database: env("DATABASE"),
    };

    let calls = Calls::default();

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Ok(region) = std::env::var("AWS_REGION") {
        loader = loader.region(aws_config::Region::new(region));
    }
    let sdk_config = loader.load().await;
    let client = RdsClient::from_conf(
        aws_sdk_rdsdata::config::Builder::from(&sdk_config)
            .interceptor(calls.clone())
            .build(),
    );

    // The proxy runs here, on this runtime, reached over loopback -- the same
    // path a local client takes, minus a process boundary the kernel would not
    // charge much for anyway.
    let api = DataApi::new(
        client.clone(),
        target.cluster_arn.clone(),
        target.secret_arn.clone(),
        target.database.clone(),
        Duration::from_secs(120),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let factory = Arc::new(ProxyFactory::new(api, "17.0".to_string()));
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let factory = factory.clone();
            tokio::spawn(async move {
                let _ = pgwire::tokio::process_socket(socket, None, factory).await;
            });
        }
    });

    let conn_str = format!(
        "host=127.0.0.1 port={port} user=bench dbname={}",
        target.database
    );
    let (pg, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
        .await
        .expect("connect to the proxy");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // Wake a cluster scaled to zero, resolve credentials, and open the TLS
    // connection the SDK will then keep. None of that is per-query cost.
    // The proxy goes first on purpose: it is the only one of the two with a
    // retry loop for a cluster resuming from zero capacity.
    eprintln!("warming up (a paused cluster takes 10-30s to wake)...");
    pg.simple_query(TINY).await.expect("warm simple query");
    pg.query(TINY, &[]).await.expect("warm extended query");
    pg.query(WIDE, &[]).await.expect("warm wide query");
    for _ in 0..3 {
        direct(&client, &target, TINY, true).await;
    }

    // -- one small row, interleaved ------------------------------------------

    let mut d_plain = Stat::new("Data API direct");
    let mut d_meta = Stat::new("Data API direct, +result metadata");
    let mut p_simple = Stat::new("proxy, simple query");
    let mut p_ext = Stat::new("proxy, extended, parsed every time");
    let mut p_prep = Stat::new("proxy, extended, statement reused");

    let tiny_stmt = pg.prepare(TINY).await.expect("prepare tiny");

    eprintln!("measuring {ITERS} rounds of `{TINY}`...");
    for _ in 0..ITERS {
        measure!(d_plain, calls, direct(&client, &target, TINY, false).await);
        measure!(
            p_simple,
            calls,
            pg.simple_query(TINY).await.expect("simple")
        );
        measure!(p_ext, calls, pg.query(TINY, &[]).await.expect("extended"));
        // These two are adjacent so `paired` has neighbouring samples.
        measure!(d_meta, calls, direct(&client, &target, TINY, true).await);
        measure!(
            p_prep,
            calls,
            pg.query(&tiny_stmt, &[]).await.expect("prepared")
        );
    }

    // -- a thousand rows, interleaved ----------------------------------------

    let mut d_wide = Stat::new("Data API direct, +result metadata");
    let mut p_wide = Stat::new("proxy, extended, statement reused");
    let wide_stmt = pg.prepare(WIDE).await.expect("prepare wide");

    eprintln!("measuring {ITERS} rounds of a 1000-row query...");
    for _ in 0..ITERS {
        measure!(d_wide, calls, direct(&client, &target, WIDE, true).await);
        measure!(
            p_wide,
            calls,
            pg.query(&wide_stmt, &[]).await.expect("wide")
        );
    }

    // -- the first sight of a statement --------------------------------------

    let mut cold = Stat::new("proxy, Describe of an unseen statement");
    let mut warm = Stat::new("proxy, Describe of a cached statement");

    eprintln!("measuring the describe probe...");
    for i in 0..ITERS {
        // Unique text, so the proxy's per-connection shape cache cannot help.
        let sql = format!("select {i} as n, 'probe'::text as t");
        measure!(cold, calls, pg.prepare(&sql).await.expect("cold prepare"));
        measure!(warm, calls, pg.prepare(&sql).await.expect("warm prepare"));
    }

    // -- opening a connection -------------------------------------------------

    let mut conn = Stat::new("proxy, connect and handshake");

    eprintln!("measuring connection setup...");
    for _ in 0..10 {
        let before = calls.get();
        let start = Instant::now();
        let (c, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
            .await
            .expect("connect to the proxy");
        conn.times.push(start.elapsed());
        conn.calls += calls.get() - before;
        drop(c);
        drop(connection);
    }

    // -- the report ------------------------------------------------------------

    println!("\n=== what the proxy costs, {ITERS} samples per row ===");
    table(
        "one row, no server-side work",
        &[&d_plain, &d_meta, &p_simple, &p_ext, &p_prep],
    );
    table("1000 rows, two columns", &[&d_wide, &p_wide]);
    table("preparing a statement", &[&cold, &warm]);
    table("opening a connection", &[&conn]);

    let (tiny_med, tiny_p90) = paired(&p_prep, &d_meta);
    let (wide_med, wide_p90) = paired(&p_wide, &d_wide);
    println!("\nproxy minus direct, subtracted round by round:");
    println!("  one row     median {tiny_med:+.2} ms, p90 {tiny_p90:+.2} ms");
    println!("  1000 rows   median {wide_med:+.2} ms, p90 {wide_p90:+.2} ms");
    println!(
        "  a single Data API call took {} ms here, so anything under about a\n  \
         millisecond is this network, not the proxy.",
        ms(d_meta.pct(0.5)),
    );
    println!(
        "\nfirst sight of a statement costs {:.0} extra Data API calls: {} ms here.\n\
         Every later use of the same text on the same connection costs {} ms and none.",
        cold.calls_per_op(),
        ms(cold.pct(0.5)),
        ms(warm.pct(0.5)),
    );
}
