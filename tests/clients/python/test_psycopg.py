"""psycopg 3 against the proxy.

psycopg builds on libpq, which sends Parse, Bind, Describe(portal) and Execute
in one go and never describes the statement. It also asks for binary results
for the types it knows, so this exercises the proxy's binary encoder from a
second, independent implementation.

Usage: PGPORT=55432 python test_psycopg.py
"""

import os
import sys
import datetime
import decimal

import psycopg

PORT = int(os.environ.get("PGPORT", "55432"))
DATABASE = os.environ.get("DATABASE", "postgres")
TABLE = f"py_test_{os.getpid()}"

failures = 0


def check(name):
    def decorator(fn):
        global failures
        try:
            fn()
            print(f"ok   {name}")
        except Exception as exc:  # noqa: BLE001 - a test runner reports everything
            failures += 1
            print(f"FAIL {name}\n     {type(exc).__name__}: {exc}")
        return fn

    return decorator


conn = psycopg.connect(
    host="127.0.0.1", port=PORT, user="test", password="anything-goes", dbname=DATABASE
)
conn.autocommit = True


@check("simple query")
def _():
    with conn.cursor() as cur:
        cur.execute("SELECT 1 AS one")
        assert cur.fetchone() == (1,)


@check("parameterised query infers its types")
def _():
    # `%s` becomes `$1` on the wire with no declared type, so the proxy has to
    # discover that it is an integer.
    with conn.cursor() as cur:
        cur.execute("SELECT %s::int + 1 AS result", (41,))
        assert cur.fetchone() == (42,)


@check("setup")
def _():
    with conn.cursor() as cur:
        cur.execute(
            f"""CREATE TABLE {TABLE} (
                   id serial PRIMARY KEY, name text NOT NULL, amount numeric(12,2),
                   ok boolean, tags text[], created_at timestamptz DEFAULT now())"""
        )


@check("insert with parameters runs exactly once")
def _():
    with conn.cursor() as cur:
        cur.execute(
            f"INSERT INTO {TABLE}(name, amount, ok) VALUES (%s, %s, %s)",
            ("alpha", decimal.Decimal("1.50"), True),
        )
        assert cur.rowcount == 1
        cur.execute(f"SELECT count(*) FROM {TABLE}")
        assert cur.fetchone()[0] == 1, "the insert must not be performed twice"


@check("insert ... returning runs exactly once")
def _():
    with conn.cursor() as cur:
        cur.execute(
            f"INSERT INTO {TABLE}(name, amount) VALUES (%s, %s) RETURNING id, name",
            ("beta", decimal.Decimal("-2.25")),
        )
        row = cur.fetchone()
        assert row[1] == "beta", row
        cur.execute(f"SELECT count(*) FROM {TABLE}")
        assert cur.fetchone()[0] == 2, "the insert must not be performed twice"


@check("types survive the round trip")
def _():
    with conn.cursor() as cur:
        cur.execute(
            f"SELECT id, name, amount, ok, created_at FROM {TABLE} WHERE name = %s",
            ("alpha",),
        )
        row = cur.fetchone()
        assert isinstance(row[0], int), row
        assert row[1] == "alpha"
        assert row[2] == decimal.Decimal("1.50"), row[2]
        assert row[3] is True
        assert isinstance(row[4], datetime.datetime), row[4]
        assert row[4].tzinfo is not None, "timestamptz must arrive with a zone"


@check("arrays survive the round trip")
def _():
    with conn.cursor() as cur:
        cur.execute(f"UPDATE {TABLE} SET tags = %s WHERE name = %s", (["a", "b"], "alpha"))
        cur.execute(f"SELECT tags FROM {TABLE} WHERE name = %s", ("alpha",))
        assert cur.fetchone()[0] == ["a", "b"]


@check("timestamptz keeps its instant")
def _():
    with conn.cursor() as cur:
        cur.execute("SELECT '2024-01-15 12:34:56+09'::timestamptz")
        got = cur.fetchone()[0]
        expected = datetime.datetime(
            2024, 1, 15, 3, 34, 56, tzinfo=datetime.timezone.utc
        )
        assert got == expected, f"{got} != {expected}"


@check("null parameters")
def _():
    with conn.cursor() as cur:
        cur.execute("SELECT %s::text", (None,))
        assert cur.fetchone()[0] is None


@check("transaction commits and rolls back")
def _():
    conn.autocommit = False
    try:
        with conn.cursor() as cur:
            cur.execute(f"INSERT INTO {TABLE}(name) VALUES (%s)", ("kept",))
        conn.commit()
        with conn.cursor() as cur:
            cur.execute(f"INSERT INTO {TABLE}(name) VALUES (%s)", ("dropped",))
        conn.rollback()
        with conn.cursor() as cur:
            cur.execute(f"SELECT count(*) FROM {TABLE} WHERE name = 'kept'")
            assert cur.fetchone()[0] == 1
            cur.execute(f"SELECT count(*) FROM {TABLE} WHERE name = 'dropped'")
            assert cur.fetchone()[0] == 0
        conn.commit()
    finally:
        conn.autocommit = True


@check("errors carry their sqlstate")
def _():
    try:
        with conn.cursor() as cur:
            cur.execute("SELECT * FROM no_such_table_here")
    except psycopg.errors.UndefinedTable:
        return
    raise AssertionError("expected UndefinedTable")


@check("server-side cursor")
def _():
    # A named cursor uses DECLARE/FETCH, which the Data API runs as ordinary
    # statements inside the transaction.
    conn.autocommit = False
    try:
        with conn.cursor(name="c1") as cur:
            cur.execute(f"SELECT name FROM {TABLE} ORDER BY name")
            rows = cur.fetchall()
            assert len(rows) >= 2, rows
        conn.commit()
    finally:
        conn.autocommit = True


@check("teardown")
def _():
    with conn.cursor() as cur:
        cur.execute(f"DROP TABLE {TABLE}")


conn.close()
print("\nall psycopg checks passed" if failures == 0 else f"\n{failures} failed")
sys.exit(0 if failures == 0 else 1)
