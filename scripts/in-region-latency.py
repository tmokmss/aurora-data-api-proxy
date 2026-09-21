#!/usr/bin/env python3
"""Time the RDS Data API from wherever this runs.

    python3 in-region-latency.py <db-cluster-identifier> [database]

Meant for CloudShell opened in the cluster's own region, which is the cheapest
way to see a Data API call with the distance taken out of it: no host to
create, no credentials to move, and the identity is already there.

It resolves the cluster and its managed secret itself, measures a bare TCP
handshake to the regional endpoint, and times `ExecuteStatement` against the
same cluster. Subtracting the first from the second leaves the service's own
work -- authenticating, resolving the secret, reaching the cluster, running the
statement and serialising the result.

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
    within a millisecond, so subtracting it from a call leaves the service.
    """
    sock = socket.socket()
    sock.settimeout(5)
    start = time.perf_counter()
    sock.connect((host, 443))
    elapsed = (time.perf_counter() - start) * 1000
    sock.close()
    return elapsed


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

    # A cluster scaled to zero takes 10-30 seconds to wake. That is a real code
    # path, but it is not what is being measured, so it happens before the
    # timing starts.
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
    for _ in range(3):
        call(TINY, True)

    endpoint = f"rds-data.{region}.amazonaws.com"

    rtts, plain, meta, wide = [], [], [], []
    print(f"measuring {ITERS} rounds...")
    for _ in range(ITERS):
        # The handshake is taken next to the calls rather than in a batch of
        # its own, so that subtracting one from the other compares samples that
        # saw the same network instead of two different minutes of it.
        rtts.append(tcp_rtt(endpoint))
        for samples, sql, metadata in (
            (plain, TINY, False),
            (meta, TINY, True),
            (wide, WIDE, True),
        ):
            start = time.perf_counter()
            call(sql, metadata)
            samples.append((time.perf_counter() - start) * 1000)

    print(f"\n=== the Data API, measured from here, region {region} ===\n")
    header()
    row("TCP round trip to the endpoint", rtts)
    row("ExecuteStatement, one row", plain)
    row("ExecuteStatement, one row, +metadata", meta)
    row("ExecuteStatement, 1000 rows, +metadata", wide)

    paired = sorted(m - r for m, r in zip(meta, rtts))
    print(
        f"\nthe service's own work, with the round trip subtracted:"
        f"\n  paired, round by round    median {pct(paired, 0.5):.1f} ms,"
        f" p90 {pct(paired, 0.9):.1f} ms"
        f"\n  floor to floor            {min(meta) - min(rtts):.1f} ms"
        f"\n\nserialising 1000 rows instead of 1 adds"
        f" {pct(wide, 0.5) - pct(meta, 0.5):.1f} ms."
        f"\nThe proxy's describe probe is five of these calls, once per"
        f" statement per connection."
    )


if __name__ == "__main__":
    main()
