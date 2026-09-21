-- psql against the proxy.
--
-- psql is the client people reach for first, and the one whose meta-commands
-- stress the parts of pg_catalog the Data API struggles with. Run with
-- ON_ERROR_STOP=1, so any failure fails the suite.

\set ON_ERROR_STOP on
\timing off

\echo '-- basics'
SELECT 1 AS one, 'hi' AS greeting;

\echo '-- multi-statement simple query'
SELECT 1 AS first; SELECT 2 AS second;

\echo '-- table lifecycle'
DROP TABLE IF EXISTS psql_smoke;
CREATE TABLE psql_smoke (
    id         serial PRIMARY KEY,
    name       text NOT NULL,
    amount     numeric(12,2),
    ok         boolean,
    tags       text[],
    created_at timestamptz DEFAULT now()
);

INSERT INTO psql_smoke(name, amount, ok, tags) VALUES ('alpha', 1.50, true, ARRAY['a','b']);
INSERT INTO psql_smoke(name, amount) VALUES ('beta', -2.25) RETURNING id, name, amount;

SELECT * FROM psql_smoke ORDER BY id;
UPDATE psql_smoke SET amount = 9.99 WHERE name = 'alpha';
DELETE FROM psql_smoke WHERE name = 'beta';
SELECT count(*) AS remaining, sum(amount) AS total FROM psql_smoke;

\echo '-- types'
SELECT 42::int2 AS i2, 42::int8 AS i8, 1.5::float8 AS f8,
       'x'::char(3) AS c, '{"k":1}'::jsonb AS j,
       decode('dead','hex') AS b,
       '550e8400-e29b-41d4-a716-446655440000'::uuid AS u,
       NULL::text AS n;

\echo '-- timestamptz carries its offset'
SELECT '2024-01-15 12:34:56+09'::timestamptz AS ts;

\echo '-- types delivered as text rather than mislabelled'
SELECT '1 day 2 hours'::interval AS i, '192.168.1.1'::inet AS ip;

\echo '-- infinity, which the Data API cannot return directly'
SELECT 'infinity'::timestamptz AS inf;

\echo '-- transactions'
BEGIN;
INSERT INTO psql_smoke(name) VALUES ('kept');
SAVEPOINT sp1;
INSERT INTO psql_smoke(name) VALUES ('discarded');
ROLLBACK TO SAVEPOINT sp1;
COMMIT;
SELECT name FROM psql_smoke ORDER BY name;

BEGIN;
INSERT INTO psql_smoke(name) VALUES ('rolled back');
ROLLBACK;
SELECT count(*) AS after_rollback FROM psql_smoke;

\echo '-- cursors, the way to read past the 1 MB result cap'
BEGIN;
DECLARE c1 CURSOR FOR SELECT generate_series(1, 5) AS n;
FETCH 2 FROM c1;
FETCH 2 FROM c1;
CLOSE c1;
COMMIT;

\echo '-- meta-commands'
\d psql_smoke
\dt
\du
\dn

\echo '-- cleanup'
DROP TABLE psql_smoke;

\echo '-- psql smoke test passed'
