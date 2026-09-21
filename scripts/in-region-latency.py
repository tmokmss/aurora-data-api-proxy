#!/usr/bin/env python3
"""Time the RDS Data API from wherever this runs.

    python3 in-region-latency.py <db-cluster-identifier> [database]

Meant for CloudShell opened in the cluster's own region, which is the cheapest
way to see a Data API call with the distance taken out of it: no host to
create, no credentials to move, and the identity is already there.

It resolves the cluster and its managed secret itself, measures a bare TCP
handshake to the regional endpoint, and times `ExecuteStatement` against the
same cluster.

Subtracting the handshake from the call is supposed to leave the service's own
work, but that subtraction is only honest if nothing else is hiding in the
wall clock. Three things can be, and each is measured rather than assumed:

* **This machine's own CPU.** Signing, TLS and JSON parsing are Python, and on
  a small or throttled host they are not free. Every call is timed twice, once
  on the wall clock and once on the process clock, so the client's share is
  visible instead of being attributed to the service.
* **Drift.** A Serverless v2 cluster scaling up from zero gets faster during
  the run. The first ten rounds are reported against the last ten, so a moving
  baseline shows up as a moving baseline.
* **Position in the round.** The three statements are rotated, so that being
  measured first is not always the same statement's misfortune, and the timings
  by position are reported so that a position effect cannot masquerade as a
  difference between statements.

The statements only read, and nothing is created.
"""

import socket
import sys
import time

import boto3
from botocore.config import Config

ITERS = 40

# One row, nothing for the server to do: what is left is the service.
TINY = "select 1 as n"

# A thousand rows, to see what serialising a result actually costs.
WIDE = "select g as n, g::text as t from generate_series(1, 1000) g"


def pct(samples, p):
    ordered = sorted(samples)
    return ordered[round((len(ordered) - 1) * p)]


def header():
    print(f"  {'':<40} {'n':>3} {'p50 ms':>8} {'p90 ms':>8} {'min ms':>8}")


def row(label, samples):
    print(
        f"  {label:<40} {len(samples):>3} "
        f"{pct(samples, 0.5):>8.1f} {pct(samples, 0.9):>8.1f} {min(samples):>8.1f}"
    )


def tcp_rtt(host):
    """One TCP handshake: the distance, with nothing else in it.

    A warm HTTPS request to this endpoint costs the same as this handshake to
    within a millisecond, so subtracting it from a call leaves the service --
    and whatever the client spent on its own CPU, which is why that is measured
    separately.
    """
    sock = socket.socket()
    sock.settimeout(5)
    start = time.perf_counter()
    sock.connect((host, 443))
    elapsed = (time.perf_counter() - start) * 1000
    sock.close()
    return elapsed


def cpu_reference():
    """A fixed lump of pure-Python work, so two machines can be compared.

    botocore's per-call cost is interpreter time rather than cryptography, so
    an integer loop resembles it more closely than a hash would.
    """
    start = time.perf_counter()
    total = 0
    for i in range(2_000_000):
        total += i * i % 7
    return (time.perf_counter() - start) * 1000, total


def main():
    if not 2 <= len(sys.argv) <= 3:
        sys.exit("usage: in-region-latency.py <db-cluster-identifier> [database]")
    cluster_id = sys.argv[1]
    database = sys.argv[2] if len(sys.argv) == 3 else "postgres"

    session = boto3.Session()
    region = session.region_name
    if not region:
        sys.exit("no region configured; open CloudShell in the cluster's region")

    cluster = session.client("rds").describe_db_clusters(
        DBClusterIdentifier=cluster_id
    )["DBClusters"][0]
    cluster_arn = cluster["DBClusterArn"]
    secret_arn = cluster.get("MasterUserSecret", {}).get("SecretArn")
    if not secret_arn:
        sys.exit(
            "the cluster has no managed master password secret, so this script "
            "cannot find the one the Data API should use"
        )

    print(f"region {region}, cluster {cluster_id}, database {database}")
    print(f"python {sys.version.split()[0]}, boto3 {boto3.__version__}")

    # `standard` rather than the default, so a retry cannot quietly inflate a
    # sample without the mode being stated.
    data = session.client(
        "rds-data", config=Config(retries={"mode": "standard", "max_attempts": 3})
    )

    def call(sql, metadata):
        return data.execute_statement(
            resourceArn=cluster_arn,
            secretArn=secret_arn,
            database=database,
            sql=sql,
            includeResultMetadata=metadata,
        )

    # A cluster scaled to zero takes 10-30 seconds to wake, and then goes on
    # getting faster as it scales. Neither is what is being measured, so the
    # warm-up is longer than it looks like it needs to be.
    print("warming up (a paused cluster takes 10-30s to wake)...")
    deadline = time.time() + 180
    while True:
        try:
            call(TINY, True)
            break
        except data.exceptions.DatabaseResumingException:
            if time.time() > deadline:
                raise
            time.sleep(2)
    for _ in range(10):
        call(TINY, True)
        call(WIDE, True)

    endpoint = f"rds-data.{region}.amazonaws.com"

    variants = [
        ("ExecuteStatement, one row", TINY, False),
        ("ExecuteStatement, one row, +metadata", TINY, True),
        ("ExecuteStatement, 1000 rows, +metadata", WIDE, True),
    ]
    wall = {name: [] for name, _, _ in variants}
    cpu = {name: [] for name, _, _ in variants}
    by_position = [[] for _ in variants]
    rtts = []

    print(f"measuring {ITERS} rounds...")
    for i in range(ITERS):
        # The handshake is taken next to the calls rather than in a batch of
        # its own, so that subtracting one from the other compares samples that
        # saw the same network instead of two different minutes of it.
        rtts.append(tcp_rtt(endpoint))

        # Rotated, so that no one statement is always the first of a round.
        shift = i % len(variants)
        order = variants[shift:] + variants[:shift]
        for position, (name, sql, metadata) in enumerate(order):
            started = time.perf_counter()
            spent = time.process_time()
            call(sql, metadata)
            elapsed = (time.perf_counter() - started) * 1000
            used = (time.process_time() - spent) * 1000
            wall[name].append(elapsed)
            cpu[name].append(used)
            by_position[position].append(elapsed)

    reference, _ = cpu_reference()

    print(f"\n=== the Data API, measured from here, region {region} ===\n")
    header()
    row("TCP round trip to the endpoint", rtts)
    for name, _, _ in variants:
        row(name, wall[name])

    print("\nof that wall time, spent on this machine's own CPU:")
    for name, _, _ in variants:
        print(f"  {name:<40} p50 {pct(cpu[name], 0.5):>7.1f} ms")
    print(f"  {'a fixed 2M-iteration Python loop':<40}     {reference:>7.1f} ms")

    print("\ndrift, median of the first ten rounds against the last ten:")
    for name, _, _ in variants:
        first = pct(wall[name][:10], 0.5)
        last = pct(wall[name][-10:], 0.5)
        print(f"  {name:<40} {first:>7.1f} -> {last:>7.1f} ms")

    print("\nby position in the round, with the statements rotated:")
    for position, samples in enumerate(by_position, start=1):
        print(f"  call {position} of 3{'':<30} p50 {pct(samples, 0.5):>7.1f} ms")

    meta = wall["ExecuteStatement, one row, +metadata"]
    meta_cpu = cpu["ExecuteStatement, one row, +metadata"]
    wide = wall["ExecuteStatement, 1000 rows, +metadata"]
    paired = sorted(m - r for m, r in zip(meta, rtts))

    print("\nwhat is left after the round trip is taken away:")
    print(
        f"  paired, round by round    median {pct(paired, 0.5):>7.1f} ms,"
        f" p90 {pct(paired, 0.9):.1f} ms"
    )
    print(f"  floor to floor            {min(meta) - min(rtts):>7.1f} ms")
    print(
        f"  of which this client      {pct(meta_cpu, 0.5):>7.1f} ms"
        "   <- not the service"
    )
    print(
        f"\nserialising 1000 rows instead of 1 adds"
        f" {pct(wide, 0.5) - pct(meta, 0.5):.1f} ms."
    )
    if pct(wide, 0.5) < pct(meta, 0.5):
        print(
            "  That is negative, which cannot be true of the database. Check the\n"
            "  drift and position lines above before believing any number here."
        )


if __name__ == "__main__":
    main()
