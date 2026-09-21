// node-postgres against the proxy.
//
// This driver drives the extended protocol the other way round from
// tokio-postgres: it sends Parse, Bind, Describe(portal) and Execute together,
// without ever asking to describe the statement. That makes it the client the
// proxy's "run it during Describe, replay it during Execute" path exists for --
// and the one that would run a statement twice if that path were wrong.
//
// Usage: PGPORT=55432 node test.mjs

import pg from 'pg';
import assert from 'node:assert/strict';

const port = Number(process.env.PGPORT ?? 55432);
const table = `node_test_${process.pid}`;
let failures = 0;

async function check(name, fn) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (err) {
    failures += 1;
    console.log(`FAIL ${name}\n     ${err.message}`);
  }
}

const client = new pg.Client({
  host: '127.0.0.1',
  port,
  user: 'test',
  password: 'anything-goes',
  database: process.env.DATABASE ?? 'postgres',
});
await client.connect();

await check('simple query', async () => {
  const r = await client.query('SELECT 1 AS one');
  assert.equal(r.rows[0].one, 1);
});

await check('parameterised query infers its types', async () => {
  // `$1` has no declared type: the proxy has to work out that comparing it to
  // an integer means it is one, or the Data API binds it as text and fails.
  const r = await client.query('SELECT $1::int + 1 AS result', [41]);
  assert.equal(r.rows[0].result, 42);
});

await check('setup', async () => {
  await client.query(
    `CREATE TABLE ${table} (id serial PRIMARY KEY, name text NOT NULL,
       amount numeric(12,2), ok boolean, tags text[], created_at timestamptz DEFAULT now())`,
  );
});

await check('insert with parameters runs exactly once', async () => {
  const r = await client.query(
    `INSERT INTO ${table}(name, amount, ok) VALUES ($1, $2, $3)`,
    ['alpha', '1.50', true],
  );
  assert.equal(r.rowCount, 1);
  const count = await client.query(`SELECT count(*)::int AS n FROM ${table}`);
  assert.equal(count.rows[0].n, 1, 'the insert must not be performed twice');
});

await check('insert ... returning runs exactly once', async () => {
  const r = await client.query(
    `INSERT INTO ${table}(name, amount) VALUES ($1, $2) RETURNING id, name, amount`,
    ['beta', '-2.25'],
  );
  assert.equal(r.rows.length, 1);
  assert.equal(r.rows[0].name, 'beta');
  assert.equal(r.rows[0].amount, '-2.25');
  const count = await client.query(`SELECT count(*)::int AS n FROM ${table}`);
  assert.equal(count.rows[0].n, 2, 'the insert must not be performed twice');
});

await check('types survive the round trip', async () => {
  const r = await client.query(`SELECT * FROM ${table} WHERE name = $1`, ['alpha']);
  const row = r.rows[0];
  assert.equal(typeof row.id, 'number');
  assert.equal(row.name, 'alpha');
  assert.equal(row.amount, '1.50');
  assert.equal(row.ok, true);
  assert.ok(row.created_at instanceof Date);
});

await check('arrays survive the round trip', async () => {
  await client.query(`UPDATE ${table} SET tags = $1 WHERE name = $2`, [['a', 'b'], 'alpha']);
  const r = await client.query(`SELECT tags FROM ${table} WHERE name = $1`, ['alpha']);
  assert.deepEqual(r.rows[0].tags, ['a', 'b']);
});

await check('timestamptz keeps its instant', async () => {
  // The Data API drops the zone; without the proxy putting it back, node would
  // read this in the local zone and get a different moment.
  const r = await client.query(`SELECT '2024-01-15 12:34:56+09'::timestamptz AS ts`);
  assert.equal(r.rows[0].ts.toISOString(), '2024-01-15T03:34:56.000Z');
});

await check('null parameters', async () => {
  const r = await client.query(`SELECT $1::text AS v`, [null]);
  assert.equal(r.rows[0].v, null);
});

await check('transaction commits', async () => {
  await client.query('BEGIN');
  await client.query(`INSERT INTO ${table}(name) VALUES ($1)`, ['kept']);
  await client.query('COMMIT');
  const r = await client.query(`SELECT count(*)::int AS n FROM ${table} WHERE name = 'kept'`);
  assert.equal(r.rows[0].n, 1);
});

await check('transaction rolls back', async () => {
  await client.query('BEGIN');
  await client.query(`INSERT INTO ${table}(name) VALUES ($1)`, ['dropped']);
  await client.query('ROLLBACK');
  const r = await client.query(`SELECT count(*)::int AS n FROM ${table} WHERE name = 'dropped'`);
  assert.equal(r.rows[0].n, 0);
});

await check('errors carry their sqlstate', async () => {
  await assert.rejects(
    () => client.query('SELECT * FROM no_such_table_here'),
    (err) => err.code === '42P01',
  );
});

await check('prepared (named) statements', async () => {
  const r = await client.query({
    name: 'find-by-name',
    text: `SELECT id, name FROM ${table} WHERE name = $1`,
    values: ['alpha'],
  });
  assert.equal(r.rows[0].name, 'alpha');
  // Reusing the name goes straight to Bind, with no second Parse.
  const again = await client.query({
    name: 'find-by-name',
    text: `SELECT id, name FROM ${table} WHERE name = $1`,
    values: ['beta'],
  });
  assert.equal(again.rows[0].name, 'beta');
});

await check('teardown', async () => {
  await client.query(`DROP TABLE ${table}`);
});

await client.end();
console.log(failures === 0 ? '\nall node-postgres checks passed' : `\n${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
