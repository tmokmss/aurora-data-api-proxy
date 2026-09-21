# aurora-data-api-proxy

Connect `psql`, a GUI client, or any PostgreSQL driver to an Aurora Serverless
cluster over the RDS Data API — no bastion host, no VPN, no security-group
holes. Just IAM.

```console
$ aurora-data-api-proxy \
    --cluster-arn arn:aws:rds:us-east-1:123456789012:cluster:my-cluster \
    --secret-arn  arn:aws:secretsmanager:us-east-1:123456789012:secret:my-secret \
    --database    postgres

$ psql -h 127.0.0.1 -p 5432 -U postgres
psql (18.3, server 17.9)
postgres=> select * from orders limit 3;
```

The proxy speaks the PostgreSQL wire protocol on a local socket and turns each
query into an `ExecuteStatement` HTTP call. Your client believes it is talking
to PostgreSQL; the only credentials in play are the AWS ones the proxy resolves
for itself.

## Why

The Data API already solves the "reach a private Aurora cluster over IAM"
problem — but only for code you can rewrite. In JavaScript,
[`data-api-client`](https://github.com/jeremydaly/data-api-client) slots in at
the driver layer and everything above it carries on unchanged. There is nowhere
to slot such a shim into `psql`, DBeaver, DataGrip, `pg_dump`, or the drivers
for Python, Go, Rust and Java.

This proxy puts the compatibility layer one level lower, at the wire protocol,
so those clients work without knowing the Data API exists.

## Install

```console
cargo install --path .
```

## Configuration

Every option can be a flag or an environment variable.

| Flag | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `--cluster-arn` | `CLUSTER_ARN` | *required* | The Aurora cluster to query |
| `--secret-arn` | `SECRET_ARN` | *required* | Secrets Manager secret with the database credentials |
| `--database` | `DATABASE` | *required* | Database to connect to |
| `--listen` | `LISTEN` | `127.0.0.1:5432` | Address to listen on |
| `--region` | `AWS_REGION` | from the credential chain | AWS region |
| `--resume-timeout-secs` | `RESUME_TIMEOUT_SECS` | `90` | How long to wait for a scaled-to-zero cluster to wake |
| `--log-level` | `LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` or `trace` |

AWS credentials come from the standard chain: environment, `AWS_PROFILE`, SSO,
instance or task role. The cluster needs the Data API enabled
(`aws rds modify-db-cluster --db-cluster-identifier <id> --enable-http-endpoint`),
and the caller needs `rds-data:*` on the cluster plus
`secretsmanager:GetSecretValue` on the secret.

### A word on security

**The proxy accepts any password, and binds to loopback for that reason.** The
credentials that protect your data are the AWS ones; a password from the client
would secure nothing. If you move it off loopback, anyone who can reach the port
can use your AWS credentials to query the cluster. The proxy warns loudly when
you do.

It is built for a developer's machine or an application sidecar — one client,
one process — not as a shared central proxy.

## How it works

The Data API is stateless: one SQL string in, one result out. It has no
connections, no prepared statements, and nothing resembling the wire protocol's
`Describe` message. Most clients, meanwhile, use the extended query protocol.
Closing that gap is what this program is.

**Describe(portal)** arrives after `Bind`, so the parameters are known. The
proxy runs the statement there and keeps the result, answering `Describe` from
the metadata that comes back. The `Execute` that follows replays those rows
instead of running anything — pgwire's own portal state machine guarantees a
started portal is never executed a second time.

**Describe(statement)** arrives before `Bind`, with no parameters at all, and
that is the harder half. The proxy borrows the one stateful thing the Data API
does have — a transaction, inside which the service pins a single backend
session. It `PREPARE`s the statement there, reads the parameter and result types
out of `pg_prepared_statements`, and recovers the result column names by
planning (never running) the statement:

- A read-only statement is wrapped as `SELECT * FROM (<query>) WHERE false`,
  with typed `NULL`s in place of the parameters. PostgreSQL folds that to a
  one-time false filter and never executes the inner query, but the Data API
  still returns the full column metadata.
- `INSERT`/`UPDATE`/`DELETE ... RETURNING` cannot be wrapped that way, so the
  names are read off the `RETURNING` clause. **Your DML is never executed to
  describe it.**

The whole probe runs inside a `SAVEPOINT`, because a failed statement aborts a
PostgreSQL transaction and describing a statement that does not compile is an
ordinary thing for a client to do.

**Parameters** are rewritten from PostgreSQL's `$1` to the Data API's `:p1` with
a token-level scan that respects string literals, quoted identifiers,
dollar-quoted bodies and comments. Each placeholder is wrapped in an explicit
`CAST(:p1 AS integer)`: the Data API binds every JSON string as `text` and
nothing else, so without the cast `WHERE id = $1` fails with
`operator does not exist: integer = text`. Naming the type also gives NULLs a
type and keeps a `timestamptz` parameter's zone offset intact.

**Transactions** map `BEGIN`/`COMMIT`/`ROLLBACK` onto the Data API's transaction
calls, holding the transaction id for the life of the connection. `SAVEPOINT`
and friends are forwarded as ordinary SQL. If a client disconnects mid
transaction, the proxy rolls it back rather than leaving it to expire.

**Results** are rebuilt from `columnMetadata.typeName`, in text or binary as the
client asked. Types the proxy cannot encode faithfully are reported as `text`
rather than mislabelled, so a client sees a readable value or a clean type
error, never a corrupted one.

## What works, and what does not

See [COMPATIBILITY.md](COMPATIBILITY.md) for the tested matrix. The headline
limits, all inherited from the Data API itself:

- A single result may not exceed **1 MB**. Use `LIMIT`, or a cursor.
- **Session state does not persist** outside a transaction: `SET search_path`
  reports success and then does nothing, because the next statement lands on a
  different pooled session. The proxy warns when you do it.
- **No `LISTEN`/`NOTIFY`**, no `COPY`, no multidimensional arrays.
- Statements the Data API's own parameter scanner would silently corrupt — a
  `:name` inside a dollar-quoted string, an array slice like `a[1:2]` — are
  **refused** rather than forwarded.

## Testing

Logic that needs no cluster is a unit test:

```console
cargo test --lib
```

The integration tests need a real Aurora cluster and are skipped without one:

```console
export CLUSTER_ARN=... SECRET_ARN=... DATABASE=postgres
cargo test                                   # Rust, via tokio-postgres
./scripts/test-clients.sh                    # node-postgres, psycopg, pgx, psql
```

They connect through real client libraries rather than calling the proxy's
internals, because the differences between clients — which of them describes
statements, which asks for binary, which sends `Describe(portal)` — are exactly
what this proxy has to get right.

## License

MIT
