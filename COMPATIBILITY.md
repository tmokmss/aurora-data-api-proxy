# Compatibility

Everything on this page was checked against a real cluster — Aurora PostgreSQL
**17.9**, Serverless v2 scaled to zero, Data API enabled — with the client
libraries and versions named below. Nothing here is inferred from
documentation. Where something does not work, the actual error is quoted.

Re-run it yourself with `cargo test` and `./scripts/test-clients.sh`.

## Clients

| Client | Version | Protocol it uses | Result |
| --- | --- | --- | --- |
| `psql` / libpq | 18.3 | Simple query; extended for `\d`-style lookups | Works |
| tokio-postgres (Rust) | 0.7.18 | Extended, `Describe(statement)`, binary results | Works |
| node-postgres (`pg`) | 8.23.0 | Extended, `Describe(portal)`, text results | Works |
| psycopg (Python) | 3.3.6 | Extended via libpq, `Describe(portal)`, binary | Works |
| pgx (Go) | 5.11.0 | Extended, `Describe(statement)`, binary, statement cache | Works |

These five are deliberately different from each other. tokio-postgres and pgx
send `Parse` with no parameter types and rely on `Describe(statement)` to tell
them; node-postgres and psycopg never describe a statement and send
`Describe(portal)` instead. Both halves of the hard problem are covered.

Untested, and therefore unknown: JDBC, DBeaver, DataGrip, `pg_dump`, Prisma,
Hibernate, SQLAlchemy, Django.

## SQL features

| Feature | Status | Notes |
| --- | --- | --- |
| `SELECT`, `INSERT`, `UPDATE`, `DELETE` | Works | |
| `INSERT ... RETURNING` | Works | Described without being executed |
| DDL (`CREATE`/`ALTER`/`DROP`) | Works | |
| Parameters (`$1`) in any position | Works | Rewritten to `:p1` with an explicit cast |
| NULL parameters | Works | Cast gives them a type |
| Multi-statement simple query | Works | Split by the proxy; the Data API takes one at a time |
| `BEGIN` / `COMMIT` / `ROLLBACK` | Works | Mapped onto Data API transactions |
| `SAVEPOINT`, `ROLLBACK TO`, `RELEASE` | Works | Forwarded as ordinary SQL |
| Rollback on disconnect | Works | Otherwise the transaction would hold locks until it expires |
| `DECLARE` / `FETCH` cursors | Works | Inside a transaction. The way to read results over 1 MB |
| Server-side cursors (psycopg `name=`) | Works | Needs the proxy's `Describe` on a SQL-level portal |
| CTEs, `VALUES`, `generate_series` | Works | |
| Duplicate column names (`SELECT a.id, b.id`) | Works | |
| Temporary tables | Works | Inside a transaction only — see session state |
| `SET LOCAL` in a transaction | Works | |
| `PREPARE` / `EXECUTE` in a transaction | Works | |
| `SET` outside a transaction | **Silently ineffective** | The proxy emits a `WARNING` |
| `PREPARE` outside a transaction | **Does not work** | `prepared statement "p" does not exist` |
| `LISTEN` / `NOTIFY` | **Does not work** | Accepted, but no notification can ever arrive. The proxy emits a `WARNING` |
| `COPY ... TO/FROM STDOUT` | **Does not work** | `execute: unexpected message: CopyOutResponse` |
| Multidimensional arrays | **Does not work** | `The result contains a multidimensional array` |
| Results over 1 MB | **Does not work** | `The result exceeds the size limit 1 MB` — use a cursor or `LIMIT` |
| TLS (`sslmode=prefer`, the default) | Works | The proxy answers `N` and the client falls back to plaintext |
| TLS (`sslmode=disable`) | Works | |
| TLS (`sslmode=require`) | **Does not work** | `server does not support SSL, but SSL was required`. The proxy is loopback-only, and the hop it fronts — proxy to AWS — is HTTPS |
| Query cancellation (Ctrl-C) | Not implemented | The Data API has no cancel call; the request runs to completion |

### psql meta-commands

`\d`, `\dt`, `\di`, `\ds`, `\dv`, `\dn`, `\dT`, `\df`, `\dp`, `\du`, `\l`,
`\conninfo` and `\encoding` all work.

Two of them only work because the proxy retries around Data API limits: `\d`
selects `"char"` columns from `pg_catalog`, and `\du` reads a `rolvaliduntil` of
`infinity`. Both would otherwise fail outright — see *Automatic recovery* below.

## Types

Values are rebuilt from the Data API's `columnMetadata.typeName`, in text or
binary as the client asks.

**Full fidelity, text and binary:** `bool`, `int2`, `int4`, `int8`, `float4`,
`float8`, `numeric`, `text`, `varchar`, `bpchar`, `name`, `oid`, `date`, `time`,
`timestamp`, `timestamptz`, `uuid`, `json`, `jsonb`, `bytea`, and
one-dimensional arrays of all of these.

**Delivered as `text`:** everything else — `interval`, `inet`, `cidr`, `xml`,
`"char"`, enums, composites, ranges, `tsvector`, PostGIS types. The column is
*reported* as `text`, not mislabelled, so a client either shows the value or
raises a clean type error. It never decodes a wrong value.

### Type details worth knowing

- **`timestamptz` keeps its instant.** The Data API returns these as a UTC
  wall-clock string with the zone offset stripped — `'2024-01-15 12:34:56+09'`
  comes back as `2024-01-15 03:34:56`. Passed through unchanged, a client would
  apply its own session zone and silently read a different moment. The proxy
  appends `+00` and reports `TimeZone=UTC` at startup, matching the Data API's
  own session zone.
- **`numeric` keeps its precision.** Both the value and the `numeric(p,s)` type
  modifier are preserved, so JDBC-style clients do not render spurious trailing
  zeros.
- **`serial` columns.** The Data API reports a serial column's *declared* type
  (`typeName: "serial"`), not the underlying `int4`. The proxy maps it back;
  left alone, an `id serial` column arrives as text and fails to decode.
- **`infinity` timestamps and dates cannot be returned.** The Data API fails
  with a bare `InternalFailure` and no message at all. The proxy detects this
  and retries with the temporal columns cast to text.
- **Multidimensional arrays cannot be returned** by the Data API at all.

## Statements the proxy refuses

The Data API scans SQL for its own `:name` placeholders without respecting
dollar-quoted strings or array-slice syntax. Two cases are bad enough that the
proxy rejects the statement rather than forward it:

| Construct | What the Data API does | Why it is refused |
| --- | --- | --- |
| `:name` inside `$$...$$` | `SELECT $$a :b c$$` returns `a $1 c` | **Silent corruption** — a wrong answer with no error |
| Array slice `a[1:2]` | Reads `:2` as a parameter | `syntax error at or near "$1"` |

Single-quoted strings, double-quoted identifiers and comments *are* respected by
the Data API, so `'{"a":1}'::jsonb` and `-- :note` are fine.

## Automatic recovery

Two Data API failures are retried rather than reported, because the retry is
both safe and invisible:

1. **A type the Data API will not return** (`"char"`, `interval`, `xml`,
   `regtype[]`) fails the whole query with `UnsupportedResultException`. The
   proxy asks for the result's shape — a zero-row query returns metadata without
   tripping the limit — and re-runs with those columns cast to `text`. They are
   columns it would have delivered as text anyway, so nothing is lost. This is
   what makes `\d` work.
2. **A bare `InternalFailure`**, which is what an `infinity` timestamp produces.
   The proxy re-runs with temporal columns cast to text as well. This is what
   makes `\du` work.

Both retries apply only to statements that write nothing, so nothing can run
twice.

## Performance

Every statement is an HTTP round trip rather than the sub-millisecond of a local
socket, and that round trip is nearly the whole cost: the proxy's own
translation does not show up next to it.

`cargo run --release --example overhead` measures this against a real cluster.
It runs both halves in one process, interleaved, and subtracts each round from
its neighbour so that network drift cancels instead of drowning the answer. Two
runs from outside the region, where one Data API call took 186–200 ms:

| | Data API calls | versus calling the Data API yourself |
| --- | --- | --- |
| A query, statement already seen | 1 | +0.3 ms and −0.2 ms at the median |
| The same query returning 1000 rows | 1 | +0.6 ms and +1.0 ms at the median |
| Opening a connection | 0 | 0.2 ms, no call at all |
| `Describe` of a statement seen before | 0 | 0.2 ms, no call at all |
| `Describe` of a statement not seen before | **5** | five extra round trips |

So two things cost extra, and only one of them is large:

- `Describe(statement)` runs a probe — `BEGIN`, `PREPARE`, a catalogue read, a
  shape query, `ROLLBACK` — the first time a connection sees a given statement.
  That is five extra Data API calls: the first use of a statement costs six
  round trips where a direct caller pays one. The shape is then cached per
  connection by SQL text, so every later use of that text is free. The cache
  dies with the connection, so a pool that opens a connection per request
  re-probes everything; a long-lived connection pays once. Simple queries never
  probe, because nothing asks the proxy to describe anything.
- A scaled-to-zero cluster takes 10–30 seconds to wake. The proxy retries
  `DatabaseResumingException` with backoff for `--resume-timeout-secs`.

The millisecond figures belong to one laptop and one region pairing. The call
counts do not: one call per query, five extra on a first `Describe`, is what
holds wherever it runs.
