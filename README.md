# aurora-data-api-proxy

[![CI](https://github.com/tmokmss/aurora-data-api-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/tmokmss/aurora-data-api-proxy/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/tmokmss/aurora-data-api-proxy)](https://github.com/tmokmss/aurora-data-api-proxy/releases/latest)

Connect `psql`, a GUI client, or any PostgreSQL driver to an Aurora Serverless
cluster over the RDS Data API — no bastion host, no VPN, no security-group
holes, just IAM. The proxy speaks the PostgreSQL wire protocol on a local socket
and turns each query into an `ExecuteStatement` HTTP call, so the client
believes it is talking to PostgreSQL while the only credentials in play are the
AWS ones the proxy resolves for itself. It implements the extended query
protocol and not merely simple queries, which is what makes real drivers and
ORMs work; [COMPATIBILITY.md](COMPATIBILITY.md) records, per client, what was
tested and what does not work.

## Usage

Download a binary from the
[latest release](https://github.com/tmokmss/aurora-data-api-proxy/releases/latest)
— Linux and macOS, x86_64 and arm64 — or build from source with
`cargo install --path .`.

```console
$ aurora-data-api-proxy \
    --cluster-arn arn:aws:rds:us-east-1:123456789012:cluster:my-cluster \
    --secret-arn  arn:aws:secretsmanager:us-east-1:123456789012:secret:my-secret \
    --database    postgres

$ psql -h 127.0.0.1 -p 5432 -U postgres
psql (18.3, server 17.9)
postgres=> select * from orders limit 3;
```

Every option is a flag or an environment variable:

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

> **The proxy accepts any password, and binds to loopback for that reason.**
> Your AWS credentials are what protect the data; a password from the client
> would secure nothing. Move it off `127.0.0.1` and anyone who can reach the
> port can query your cluster with your credentials — the proxy warns loudly if
> you do. It is built for a developer's machine or an application sidecar, not
> as a shared central proxy.

## Motivation

The Data API already solves the "reach a private Aurora cluster over IAM"
problem — but only for code you can rewrite. In JavaScript,
[`data-api-client`](https://github.com/jeremydaly/data-api-client) slots in at
the driver layer and everything above it carries on unchanged. There is nowhere
to slot such a shim into `psql`, DBeaver, DataGrip, `pg_dump`, or the drivers
for Python, Go, Rust and Java.

This proxy puts the compatibility layer one level lower, at the wire protocol,
so those clients work without knowing the Data API exists.

---

MIT licensed.
