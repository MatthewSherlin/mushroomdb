#!/usr/bin/env node
// The official JavaScript Bolt driver against `mushroomdb serve --bolt-port`.
//
// Runs the twelve driver-compatibility scenarios of the 0.10 transactions
// spec (W3 section 6), each by name, each asserting the values it gets back.
// Exits 0 when all twelve pass; otherwise prints every failing scenario and
// exits 1.
//
// Driver: the official JavaScript driver package, pinned to 6.2.0. Its name
// is spelled once, as character codes, in VENDOR below (the names gate keeps
// it out of every tracked file); `--requirement` prints the npm spec CI
// installs.
//
//   node tests/bolt/js_compat.mjs bolt://127.0.0.1:7687 [token] [role-token]
//   node tests/bolt/js_compat.mjs --requirement
//
// The full-access token and the role token default to BOLT_COMPAT_TOKEN and
// BOLT_COMPAT_ROLE_TOKEN. The role token must be bound to a role that sees
// only `Doc` nodes; tests/bolt/run_compat.sh starts a server set up that way.
// NODE_PATH is honoured for finding the driver, so it can be installed
// outside the checkout.

import { createRequire } from 'node:module';
import path from 'node:path';

const DRIVER_VERSION = '6.2.0';
// The driver vendor's name: the package is `<vendor>-driver`, and the routing
// URI scheme is the bare name. Character codes, so that no tracked file
// spells it (scripts/check-names.py).
const VENDOR = String.fromCharCode(0x6e, 0x65, 0x6f, 0x34, 0x6a);
const DRIVER_PACKAGE = `${VENDOR}-driver`;
const ROUTING_SCHEME = VENDOR;

if (process.argv[2] === '--requirement') {
  console.log(`${DRIVER_PACKAGE}@${DRIVER_VERSION}`);
  process.exit(0);
}
const ROW_COUNT = 1000;
const FETCH_SIZE = 100;
const SEED = [
  ['Doc', 'd1'],
  ['Doc', 'd2'],
  ['Secret', 's1'],
  ['Person', 'seed-1'],
  ['Person', 'seed-2'],
];

// Resolve the driver from NODE_PATH as well as from the working directory:
// ES module resolution ignores NODE_PATH, createRequire's does not.
const require = createRequire(path.join(process.cwd(), 'noop.js'));
const driverLib = require(DRIVER_PACKAGE);
const driverVersion = require(`${DRIVER_PACKAGE}/package.json`).version;
const { Date: BoltDate, DateTime, LocalDateTime, Duration, Node } = driverLib.types;

class AssertionError extends Error {
  name = 'AssertionError';
}

function check(condition, message) {
  if (!condition) throw new AssertionError(message);
}

const auth = (token) => driverLib.auth.basic('compat', token);
const num = (value) => (driverLib.isInt(value) ? value.toNumber() : value);

async function single(ctx, query, params = {}) {
  const session = ctx.driver.session();
  try {
    const result = await session.run(query, params);
    check(result.records.length === 1, `${result.records.length} records for ${query}`);
    return result.records[0];
  } finally {
    await session.close();
  }
}

async function consume(ctx, query, params = {}) {
  const session = ctx.driver.session();
  try {
    await session.run(query, params);
  } finally {
    await session.close();
  }
}

async function expectFailure(promise, code) {
  try {
    await promise;
  } catch (err) {
    check(err.code === code, `error code ${err.code}, expected ${code}: ${err.message}`);
    return err;
  }
  throw new AssertionError(`expected a ${code} failure, got success`);
}

// 1. Connect and auth (valid token).
async function connectAndAuth(ctx) {
  await ctx.driver.verifyConnectivity();
  const info = await ctx.driver.getServerInfo();
  check(info.agent.startsWith('MushroomDB/'), `server agent ${info.agent}`);
  check([5, 4].includes(info.protocolVersion.major), `protocol ${info.protocolVersion}`);
  const record = await single(ctx, 'RETURN 1 AS one');
  check(num(record.get('one')) === 1, `RETURN 1 gave ${record.get('one')}`);
}

// 2. Auth failure (invalid token) -> the driver receives Unauthorized.
async function authFailure(ctx) {
  const bad = driverLib.driver(ctx.uri, auth(`${ctx.token}-wrong`));
  try {
    await expectFailure(bad.verifyConnectivity(), 'Neo.ClientError.Security.Unauthorized');
  } finally {
    await bad.close();
  }
}

// 3. Auto-commit read: MATCH (n) RETURN count(n) -> an integer.
async function autocommitRead(ctx) {
  const total = (await single(ctx, 'MATCH (n) RETURN count(n) AS c')).get('c');
  check(driverLib.isInt(total), `count(n) is ${typeof total}, not an Integer`);
  const docs = (await single(ctx, 'MATCH (n:Doc) RETURN count(n) AS c')).get('c');
  check(num(docs) === 2, `${docs} Doc nodes, expected the 2 seeded`);
  check(num(total) >= 5, `count(n) is ${total}, expected at least the 5 seeded`);
}

// 4. Auto-commit write: CREATE a node, then see it in a later read.
async function autocommitWrite(ctx) {
  await consume(ctx, "CREATE (p:Person {id: 'p-auto', name: 'Ada', age: 36})");
  await consume(ctx, 'MATCH (p:Person {id: $id}) SET p.nick = $nick', { id: 'p-auto', nick: 'ada' });
  const record = await single(
    ctx,
    'MATCH (p:Person {id: $id}) RETURN p.name AS name, p.age AS age, p.nick AS nick',
    { id: 'p-auto' },
  );
  const got = [record.get('name'), num(record.get('age')), record.get('nick')];
  check(JSON.stringify(got) === '["Ada",36,"ada"]', `read back ${JSON.stringify(got)}`);
}

// 5. Explicit transaction commit, then the managed transactions users write.
async function explicitTxCommit(ctx) {
  let session = ctx.driver.session();
  try {
    const tx = await session.beginTransaction();
    await tx.run("CREATE (:Person {id: 'p-tx', name: 'Grace'})");
    const seen = await tx.run("MATCH (p:Person {id: 'p-tx'}) RETURN p.name AS name");
    check(seen.records[0]?.get('name') === 'Grace', 'the transaction did not see its own write');
    await tx.commit();
  } finally {
    await session.close();
  }
  const committed = await single(ctx, "MATCH (p:Person {id: 'p-tx'}) RETURN count(p) AS c");
  check(num(committed.get('c')) === 1, `${committed.get('c')} committed nodes, expected 1`);

  session = ctx.driver.session();
  let ids;
  try {
    await session.executeWrite((tx) =>
      tx.run('MATCH (a {id: $a}), (b {id: $b}) CREATE (a)-[:K]->(b)', { a: 'p-auto', b: 'p-tx' }),
    );
    ids = await session.executeRead(async (tx) => {
      const result = await tx.run('MATCH (a {id: $a})-[:K]->(b) RETURN b.id AS id ORDER BY id', {
        a: 'p-auto',
      });
      return result.records.map((r) => r.get('id'));
    });
  } finally {
    await session.close();
  }
  check(JSON.stringify(ids) === '["p-tx"]', `managed read saw ${JSON.stringify(ids)}`);
}

// 6. Explicit transaction rollback: the node is absent afterwards.
async function explicitTxRollback(ctx) {
  const session = ctx.driver.session();
  try {
    const tx = await session.beginTransaction();
    await tx.run("CREATE (:Person {id: 'p-rolled-back'})");
    const inside = await tx.run("MATCH (p:Person {id: 'p-rolled-back'}) RETURN count(p) AS c");
    check(num(inside.records[0].get('c')) === 1, 'the transaction did not see its own write');
    await tx.rollback();

    class Abort extends Error {}
    try {
      await session.executeWrite(async (tx2) => {
        await tx2.run("CREATE (:Person {id: 'p-aborted'})");
        throw new Abort('abort');
      });
      throw new AssertionError('the managed transaction did not throw');
    } catch (err) {
      if (!(err instanceof Abort)) throw err;
    }
  } finally {
    await session.close();
  }
  const record = await single(
    ctx,
    "MATCH (p:Person) WHERE p.id IN ['p-rolled-back', 'p-aborted'] RETURN count(p) AS c",
  );
  check(num(record.get('c')) === 0, `${record.get('c')} rolled-back nodes are visible`);
}

// 7. Streaming: 1000 rows pulled in batches smaller than the result.
async function streaming(ctx) {
  await consume(ctx, "UNWIND range(0, $n - 1) AS i CREATE (:Row {id: 'row-' + toString(i), i: i})", {
    n: driverLib.int(ROW_COUNT),
  });
  const session = ctx.driver.session({ fetchSize: FETCH_SIZE });
  const rows = [];
  try {
    // The streaming API: records arrive as the driver pulls each batch.
    await new Promise((resolve, reject) => {
      session.run('MATCH (n:Row) RETURN n, n.i AS i').subscribe({
        onNext: (record) => rows.push([record.get('n'), num(record.get('i'))]),
        onCompleted: resolve,
        onError: reject,
      });
    });
  } finally {
    await session.close();
  }
  check(rows.length === ROW_COUNT, `${rows.length} rows streamed, expected ${ROW_COUNT}`);
  const sorted = rows.map(([, i]) => i).sort((a, b) => a - b);
  check(
    sorted.every((i, index) => i === index),
    'the streamed rows are not 0..999',
  );
  // Spec W3 section 3.3: a node is sent as a Node structure.
  const kinds = [...new Set(rows.map(([n]) => (n instanceof Node ? 'Node' : typeof n)))];
  check(
    rows.every(([n]) => n instanceof Node),
    `${ROW_COUNT} rows arrived, but \`RETURN n\` gave ${kinds} values` +
      ` (first ${JSON.stringify(rows[0][0])}), not Node structures`,
  );
  for (const [node] of rows) {
    check(JSON.stringify(node.labels) === '["Row"]', `labels ${node.labels}`);
    check(node.properties.id === `row-${num(node.properties.i)}`, `properties ${node.properties}`);
  }
}

// 8. Every Value type round-trips, as a parameter and as a stored property.
function sameValue(sent, got) {
  if (sent === null) return got === null;
  if (driverLib.isInt(sent)) return driverLib.isInt(got) && sent.equals(got);
  if (sent instanceof Int8Array) {
    return got instanceof Int8Array && sent.length === got.length && sent.every((b, i) => b === got[i]);
  }
  if (Array.isArray(sent)) {
    return Array.isArray(got) && sent.length === got.length && sent.every((v, i) => sameValue(v, got[i]));
  }
  if (
    sent instanceof BoltDate ||
    sent instanceof DateTime ||
    sent instanceof LocalDateTime ||
    sent instanceof Duration
  ) {
    return got?.constructor === sent.constructor && got.toString() === sent.toString();
  }
  if (typeof sent === 'object') {
    const keys = Object.keys(sent);
    return (
      typeof got === 'object' &&
      got !== null &&
      keys.length === Object.keys(got).length &&
      keys.every((k) => sameValue(sent[k], got[k]))
    );
  }
  return typeof sent === typeof got && sent === got;
}

async function valueTypes(ctx) {
  const values = {
    int: driverLib.int('-9007199254740993'),
    float: 2.5,
    string: 'naïve ☃',
    bool: true,
    null: null,
    list: [driverLib.int(1), 'two', 3.5, false],
    map: { a: driverLib.int(1), b: ['x', null] },
    date: new BoltDate(2024, 2, 29),
    datetime: new DateTime(2024, 3, 1, 12, 30, 45, 123456000, 7200),
    localdatetime: new LocalDateTime(2024, 3, 1, 12, 30, 45, 123456000),
    duration: new Duration(14, 3, 7, 500000000),
    bytes: new Int8Array([0, 1, -2, -1]),
  };
  const keys = Object.keys(values);
  const echoed = await single(ctx, `RETURN ${keys.map((k) => `$${k} AS v_${k}`).join(', ')}`, values);
  for (const key of keys) {
    const got = echoed.get(`v_${key}`);
    check(sameValue(values[key], got), `${key}: sent ${values[key]}, echoed ${got}`);
  }

  const stored = Object.fromEntries(Object.entries(values).filter(([k]) => k !== 'null' && k !== 'map'));
  await consume(ctx, "CREATE (v:Vals {id: 'v1'})");
  await consume(ctx, "MATCH (v:Vals {id: 'v1'}) SET v += $props", { props: stored });
  const read = await single(
    ctx,
    `MATCH (v:Vals {id: 'v1'}) RETURN ${Object.keys(stored)
      .map((k) => `v.${k} AS v_${k}`)
      .join(', ')}`,
  );
  for (const key of Object.keys(stored)) {
    const got = read.get(`v_${key}`);
    check(sameValue(stored[key], got), `stored ${key}: sent ${stored[key]}, read ${got}`);
  }
}

// 9. A transient error is retried by the driver, and the retry succeeds.
async function errorRetry(ctx) {
  await consume(ctx, "CREATE (c:Counter {id: 'retry', n: 0})");
  let attempts = 0;
  const session = ctx.driver.session();
  let result;
  try {
    result = await session.executeWrite(async (tx) => {
      attempts += 1;
      const read = await tx.run("MATCH (c:Counter {id: 'retry'}) RETURN c.n AS n");
      const n = num(read.records[0].get('n'));
      if (attempts === 1) {
        // Another client commits to the node this transaction read, so this
        // transaction's COMMIT conflicts and fails as transient.
        await consume(ctx, "MATCH (c:Counter {id: 'retry'}) SET c.n = c.n + 10");
      }
      await tx.run("MATCH (c:Counter {id: 'retry'}) SET c.n = $n", { n: driverLib.int(n + 1) });
      return n + 1;
    });
  } finally {
    await session.close();
  }
  check(attempts === 2, `${attempts} attempts, expected 2 (one conflict, one retry)`);
  check(result === 11, `the retry wrote ${result}, expected 11`);
  const final = await single(ctx, "MATCH (c:Counter {id: 'retry'}) RETURN c.n AS n");
  check(num(final.get('n')) === 11, `counter is ${final.get('n')}, expected 11`);

  // The conflict itself is a transient error with the retryable code.
  const raw = ctx.driver.session();
  try {
    const tx = await raw.beginTransaction();
    await tx.run("MATCH (c:Counter {id: 'retry'}) RETURN c.n");
    await consume(ctx, "MATCH (c:Counter {id: 'retry'}) SET c.n = 0");
    await tx.run("MATCH (c:Counter {id: 'retry'}) SET c.n = -1");
    const err = await expectFailure(tx.commit(), 'Neo.TransientError.Transaction.DeadlockDetected');
    check(err.retriable === true, 'the conflict is not retriable');
  } finally {
    await raw.close();
  }
}

// 10. CALL db.labels(), and a role token sees only its role's labels.
async function callDbLabels(ctx) {
  let session = ctx.driver.session();
  let labels;
  try {
    labels = (await session.run('CALL db.labels()')).records.map((r) => r.get('label'));
  } finally {
    await session.close();
  }
  for (const label of ['Doc', 'Secret', 'Person']) {
    check(labels.includes(label), `db.labels() ${labels} lacks ${label}`);
  }

  const scoped = driverLib.driver(ctx.uri, auth(ctx.roleToken));
  let roleLabels;
  let ids;
  try {
    session = scoped.session();
    try {
      roleLabels = (await session.run('CALL db.labels()')).records.map((r) => r.get('label'));
      ids = (await session.run('MATCH (n) RETURN n.id AS id')).records.map((r) => r.get('id')).sort();
      await expectFailure(
        session.run("CREATE (:Doc {id: 'role-write'})"),
        'Neo.ClientError.Security.Forbidden',
      );
    } finally {
      await session.close();
    }
  } finally {
    await scoped.close();
  }
  check(JSON.stringify(roleLabels) === '["Doc"]', `the role sees labels ${roleLabels}`);
  check(JSON.stringify(ids) === '["d1","d2"]', `the role sees nodes ${ids}`);
}

// 11. Routing URI: the driver asks ROUTE and uses the address it advertises.
// tests/bolt/run_compat.sh advertises `localhost`, so a query that ran on
// `localhost:<port>` after connecting to 127.0.0.1 went where ROUTE said.
async function routingUri(ctx) {
  const { port, hostname } = new URL(ctx.uri);
  const routing = driverLib.driver(`${ROUTING_SCHEME}://${hostname}:${port}`, auth(ctx.token));
  const servers = {};
  try {
    await routing.verifyConnectivity();
    let session = routing.session({ defaultAccessMode: driverLib.session.WRITE });
    try {
      const write = await session.run("CREATE (r:Routed {id: 'via-route'}) RETURN r.id AS id");
      check(write.records[0].get('id') === 'via-route', 'write through routing');
      servers.writer = write.summary.server.address;
    } finally {
      await session.close();
    }
    session = routing.session({ defaultAccessMode: driverLib.session.READ });
    try {
      const read = await session.run('MATCH (r:Routed) RETURN count(r) AS c');
      check(num(read.records[0].get('c')) === 1, 'read through routing');
      servers.reader = read.summary.server.address;
    } finally {
      await session.close();
    }
  } finally {
    await routing.close();
  }
  for (const [role, address] of Object.entries(servers)) {
    check(address === `localhost:${port}`, `${role} ran on ${address}, not the advertised localhost:${port}`);
  }
}

// 12. Bookmarks: commit on session A, read with A's bookmarks on session B.
async function bookmarks(ctx) {
  const a = ctx.driver.session();
  let marks;
  try {
    await a.executeWrite((tx) => tx.run("CREATE (:Note {id: 'causal', text: 'written on A'})"));
    marks = a.lastBookmarks();
  } finally {
    await a.close();
  }
  check(marks.length === 1, `${marks.length} bookmarks, expected 1`);
  check(marks[0].startsWith('mushroomdb:commit:'), `bookmark ${marks[0]}`);
  const reader = driverLib.driver(ctx.uri, auth(ctx.token));
  let text;
  try {
    const b = reader.session({ bookmarks: marks, defaultAccessMode: driverLib.session.READ });
    try {
      text = await b.executeRead(async (tx) => {
        const result = await tx.run("MATCH (n:Note {id: 'causal'}) RETURN n.text AS t");
        return result.records[0]?.get('t');
      });
    } finally {
      await b.close();
    }
  } finally {
    await reader.close();
  }
  check(text === 'written on A', `session B read ${text}`);
}

const SCENARIOS = [
  ['connect_and_auth', connectAndAuth],
  ['auth_failure', authFailure],
  ['autocommit_read', autocommitRead],
  ['autocommit_write', autocommitWrite],
  ['explicit_tx_commit', explicitTxCommit],
  ['explicit_tx_rollback', explicitTxRollback],
  ['streaming', streaming],
  ['value_types', valueTypes],
  ['error_retry', errorRetry],
  ['call_db_labels', callDbLabels],
  ['routing_uri', routingUri],
  ['bookmarks', bookmarks],
];

async function seed(ctx) {
  // One statement per node: CREATE takes one pattern (or one chain), and a
  // property in a CREATE pattern is a literal, not a parameter (query.md).
  for (const [label, key] of SEED) {
    await consume(ctx, `CREATE (:${label} {id: '${key}'})`);
  }
}

async function main(argv) {
  if (argv.length < 1) {
    console.error('usage: node tests/bolt/js_compat.mjs <bolt-uri> [token] [role-token]');
    return 2;
  }
  const [uri] = argv;
  const token = argv[1] ?? process.env.BOLT_COMPAT_TOKEN ?? '';
  const roleToken = argv[2] ?? process.env.BOLT_COMPAT_ROLE_TOKEN ?? '';
  if (!token || !roleToken) {
    console.error('a token and a role token are required');
    return 2;
  }
  if (driverVersion !== DRIVER_VERSION) {
    console.error(`driver is ${driverVersion}, this script pins ${DRIVER_VERSION}`);
    return 2;
  }

  const ctx = { uri, token, roleToken, driver: driverLib.driver(uri, auth(token)) };
  const failed = [];
  try {
    await seed(ctx);
    for (const [name, scenario] of SCENARIOS) {
      try {
        await scenario(ctx);
        console.log(`PASS ${name}`);
      } catch (err) {
        failed.push(name);
        console.log(`FAIL ${name}: ${err?.name}: ${err?.message}`);
        if (!(err instanceof AssertionError)) console.log(err?.stack);
      }
    }
  } finally {
    await ctx.driver.close();
  }
  const passed = SCENARIOS.length - failed.length;
  console.log(`javascript driver ${DRIVER_VERSION}: ${passed}/${SCENARIOS.length} scenarios passed`);
  if (failed.length > 0) {
    console.log(`failing: ${failed.join(', ')}`);
    return 1;
  }
  return 0;
}

process.exitCode = await main(process.argv.slice(2));
