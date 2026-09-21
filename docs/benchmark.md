# Benchmark

The proxy puts a wire-protocol translation between a PostgreSQL client and the
RDS Data API. This is what that costs, measured against a real cluster rather
than estimated.

## The short answer

| | Data API calls | cost, over calling the Data API yourself |
| --- | --- | --- |
| A query, once the connection has seen the statement | 1 | nothing measurable — the median difference lands on either side of zero |
| The same query returning 1000 rows | 1 | under a millisecond, also on either side of zero |
| Opening a connection | 0 | 0.1–0.3 ms |
| `Describe` of a statement seen before | 0 | 0.1 ms |
| **`Describe` of a statement not seen before** | **5** | **five extra round trips** |

Per query there is no overhead worth quoting: an `ExecuteStatement` call is two
orders of magnitude more expensive than everything the proxy does around it. The
one real cost is the first time a connection sees a statement, and it is a cost
in round trips, not in CPU.

## Method

[`examples/overhead.rs`](../examples/overhead.rs) starts the proxy on the same
runtime it measures from and drives it over loopback, while a plain
`aws_sdk_rdsdata` client issues the same statement to the same cluster. One
process, one machine, one cluster.

### The difference is taken pairwise

A single Data API call varies by more between minutes than the proxy costs in
total. Comparing a median taken now against one taken a minute later would
measure the network and report it as the proxy.

So the two variants run adjacently inside one loop, and the report subtracts
each round from its own neighbour. The median of those differences is the
proxy's share; the drift cancels because both halves of each pair saw the same
network.

### The call counts are observed, not read off the source

An SDK interceptor on `read_before_execution` counts every operation the client
starts, and each timed block is credited with the calls made inside it. "Five
extra calls on a first `Describe`" is therefore a measurement. It has come back
as exactly `5.00` calls per operation on every run.

### What is compared

- **Data API direct** — `ExecuteStatement` with `includeResultMetadata` off,
  which is what someone calling the API by hand would write.
- **Data API direct, +result metadata** — the same call with the flag on. The
  proxy needs the metadata to build a `RowDescription`, so this is the
  like-for-like baseline, and it is the one the pairwise difference uses.
- **proxy, simple query** — the simple query protocol, as `psql` sends for an
  ordinary statement typed at the prompt. No `Describe`, so no probe.
- **proxy, extended, parsed every time** — `Parse`/`Describe`/`Bind`/`Execute`
  on every call, which is what a driver with no statement cache does.
- **proxy, extended, statement reused** — prepared once, executed many times.

## Environment

- Apple M5 Pro, macOS 26.5.2, `rustc` 1.98.1, release build.
- Aurora PostgreSQL 17.9, Serverless v2, in `us-east-1`. The client ran well
  outside that region — far enough that one Data API call took about 190 ms.
  In-region the call is much cheaper; the proxy's share of it does not change.
- 40 samples per variant, 10 for connection setup. A warm-up pass first, so
  that waking a paused cluster, resolving credentials and opening the TLS
  connection are not counted as per-query cost.

## Results

Two consecutive runs, verbatim.

### Run 1

```
=== what the proxy costs, 40 samples per row ===

one row, no server-side work
                                             n  calls/op   p50 ms   p90 ms   min ms
  Data API direct                           40      1.00    195.3    202.3    183.5
  Data API direct, +result metadata         40      1.00    192.0    200.5    182.5
  proxy, simple query                       40      1.00    188.3    201.4    183.8
  proxy, extended, parsed every time        40      1.00    188.6    200.2    182.4
  proxy, extended, statement reused         40      1.00    195.0    203.5    182.9

1000 rows, two columns
                                             n  calls/op   p50 ms   p90 ms   min ms
  Data API direct, +result metadata         40      1.00    208.5    213.7    203.6
  proxy, extended, statement reused         40      1.00    207.7    212.3    204.1

preparing a statement
                                             n  calls/op   p50 ms   p90 ms   min ms
  proxy, Describe of an unseen statement    40      5.00    928.5    950.1    890.6
  proxy, Describe of a cached statement     40      0.00      0.1      0.2      0.1

opening a connection
                                             n  calls/op   p50 ms   p90 ms   min ms
  proxy, connect and handshake              10      0.00      0.3      0.3      0.2

proxy minus direct, subtracted round by round:
  one row     median +0.17 ms, p90 +6.76 ms
  1000 rows   median -0.80 ms, p90 +3.86 ms
```

### Run 2

```
=== what the proxy costs, 40 samples per row ===

one row, no server-side work
                                             n  calls/op   p50 ms   p90 ms   min ms
  Data API direct                           40      1.00    194.9    202.3    175.2
  Data API direct, +result metadata         40      1.00    194.6    205.6    176.7
  proxy, simple query                       40      1.00    197.3    201.1    176.7
  proxy, extended, parsed every time        40      1.00    195.9    200.4    175.6
  proxy, extended, statement reused         40      1.00    197.7    210.0    176.2

1000 rows, two columns
                                             n  calls/op   p50 ms   p90 ms   min ms
  Data API direct, +result metadata         40      1.00    194.9    205.3    191.6
  proxy, extended, statement reused         40      1.00    195.7    201.2    192.8

preparing a statement
                                             n  calls/op   p50 ms   p90 ms   min ms
  proxy, Describe of an unseen statement    40      5.00    929.4   1007.6    888.2
  proxy, Describe of a cached statement     40      0.00      0.1      0.2      0.1

opening a connection
                                             n  calls/op   p50 ms   p90 ms   min ms
  proxy, connect and handshake              10      0.00      0.1      0.2      0.1

proxy minus direct, subtracted round by round:
  one row     median +0.85 ms, p90 +17.26 ms
  1000 rows   median +0.41 ms, p90 +7.84 ms
```

## Reading the numbers

**Per query, the proxy is free.** The paired median came out at +0.17 ms and
+0.85 ms for one row, and −0.80 ms and +0.41 ms for a thousand. A quantity that
changes sign between runs is not a cost; it is what is left of the network after
the subtraction. The honest statement is that the proxy's per-query share is
below what this method can resolve, which is roughly a millisecond against a
190 ms call.

**Returning a thousand rows costs no more than returning one.** Both sides
already pay to deserialize the Data API's JSON; the proxy's extra work is
encoding those values into `DataRow` messages, and it does not surface.

**The tail belongs to the network.** The p90 of the difference swung from
+3.86 ms to +17.26 ms between two runs taken minutes apart, while the medians
stayed at zero. Nothing in the proxy changed between those runs.

**Opening a connection is local.** 0.1–0.3 ms, and no Data API call at all: the
proxy answers the startup handshake by itself and does not touch the cluster
until a statement arrives.

**The first `Describe` of a statement costs five extra calls.** The Data API has
no way to report a statement's parameter and result types without running it, so
the proxy asks PostgreSQL through a transaction: `BEGIN`, `PREPARE`, a read of
`pg_prepared_statements`, a shape query, `ROLLBACK`. Six round trips where a
direct caller pays one — 930 ms at this distance, and proportionally less
in-region, but always five calls.

That cost is charged once per statement text per connection:

- A **long-lived connection** pays it on first use and never again. Later uses
  measured 0.1 ms with no Data API call.
- A **pool that opens a connection per request** pays it on every request, since
  the cache lives on the connection and dies with it.
- **Simple queries never probe.** `psql` typing a statement at the prompt sends
  no `Describe`, so this whole path is skipped.

## Where the 190 ms goes

Almost none of it is the Data API. [`scripts/in-region-latency.py`](../scripts/in-region-latency.py)
answers this by measuring one thing the harness above does not: an
**unauthenticated POST to the same endpoint on a kept-open connection**. The
service rejects it at the front door, so it carries the network, the TLS
session and the endpoint's dispatch, and none of the authenticating,
secret-resolving or querying. The gap between that and a real call is the work.

Run from a laptop far outside the region and from CloudShell inside it, within
minutes of each other, against the same cluster:

| | network floor | `ExecuteStatement` floor | the service's own work |
| --- | --- | --- | --- |
| outside the region | 170.6 ms | 179.6 ms | **9.0 ms** |
| CloudShell, in `us-east-1` | 1.5 ms | 17.4 ms | **15.9 ms** |

So a Data API call inside its own region is **about 17 ms**, observed, and the
190 ms seen from across an ocean is 172 ms of ocean and roughly ten of service.
The Data API is thin. What is expensive is the distance.

That also prices the probe: five calls is about **90 ms in-region** against the
930 ms measured from outside it.

### Why the floor, and not the median

Those figures are floors because from CloudShell the median cannot be trusted,
and the output says so itself: it reported a thousand rows arriving 19 ms
*faster* than one row, which no database does. Something in that wall clock is
neither the network nor the service.

It is the client. The script's fixed 2M-iteration reference loop took 125 ms on
one CloudShell run and 252 ms on the next — the same machine, twice the time,
which is the signature of a throttled container rather than a slow one. Time
spent waiting for the scheduler lands in the wall clock and not in
`process_time`, so it is invisible to the per-call CPU figure and arrives
looking like service latency.

A floor is immune to this, because a stall can only ever add. Both floors are
small, both agree to within single-digit milliseconds, and the remaining gap
between them (9.0 against 15.9) is about what a client two to three times
slower would spend on TLS and JSON. The medians, from a quiet machine, agree
too: 182.2 ms against a 172.2 ms baseline is 10.0 ms of service, next to the
9.0 ms floor.

None of this changes the proxy's own numbers above. Those were measured on the
quiet machine, and they are differences between two things measured on it, so
whatever the host costs cancels.

## What this does not measure

- **Concurrency.** Every number here is one statement at a time. Throughput
  under parallel load, and whether the Data API throttles it, is untested.
- **The proxy in its own region.** The ~17 ms in-region call above was measured
  with a plain Data API client in CloudShell, not with the proxy: there is no
  in-region host running it. The proxy's share should not change, since it is
  local work either way, but that is reasoning rather than a measurement.
- **Why a CloudShell median is inflated.** That it is inflated is established;
  that CFS throttling is the mechanism is the obvious candidate and was not
  proven. The floors are used instead, which does not depend on the answer.
- **Large results.** The 1 MB cap and what happens near it is a correctness
  question, covered in [COMPATIBILITY.md](../COMPATIBILITY.md), not a timing one.

## Reproducing

```console
$ AWS_PROFILE=... AWS_REGION=us-east-1 \
  CLUSTER_ARN=arn:aws:rds:... \
  SECRET_ARN=arn:aws:secretsmanager:... \
  DATABASE=postgres \
  cargo run --release --example overhead
```

CI does not run it. It needs a real cluster, and unlike the integration tests it
is meant to be run enough times to be worth the API calls it makes.

For the Data API on its own, with the distance taken out, open CloudShell in the
cluster's region — the identity is already there, and nothing has to be created:

```console
$ curl -sO https://raw.githubusercontent.com/tmokmss/aurora-data-api-proxy/main/scripts/in-region-latency.py
$ python3 in-region-latency.py <db-cluster-identifier>
```

It prints every sample, not only percentiles, which is what made the inflated
CloudShell median visible rather than believable.
