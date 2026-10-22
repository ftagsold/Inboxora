// Differential contract harness: run identical request scenarios against the Node backend and
// the Rust backend (each with its own database and Redis DB, seeded identically) and compare
// status, selected headers and normalised JSON bodies.
//
//   node tests/contract/harness.mjs [scenario-filter]
//
// Env: PGHOST/PGPORT/PGUSER/PGPASSWORD for an admin connection (databases contract_node and
// contract_rust are recreated), REDIS_HOST/REDIS_PORT, RUST_BIN (path to inboxora-server).
import { spawn, execFileSync } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { createHmac } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import pg from 'pg';
import { createClient } from 'redis';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../..');
const PGHOST = process.env.PGHOST || 'localhost';
const PGPORT = Number(process.env.PGPORT || 55432);
const PGUSER = process.env.PGUSER || 'mailflow';
const PGPASSWORD = process.env.PGPASSWORD || 'mailflow';
const REDIS = `redis://${process.env.REDIS_HOST || '127.0.0.1'}:${process.env.REDIS_PORT || 56379}`;
const SECRET = 'contract-secret-'.padEnd(48, 'x');
const KEY = '1'.repeat(64);
const RUST_BIN = process.env.RUST_BIN || path.join(root, 'backend-rs/target/debug/inboxora-server');
const PGBIN = process.env.PGBIN || '/opt/homebrew/opt/postgresql@17/bin';

export const BACKENDS = {
  node: { port: 4101, db: 'contract_node', redisDb: 1 },
  rust: { port: 4102, db: 'contract_rust', redisDb: 2 },
};

function env(b) {
  return {
    ...process.env,
    PORT: String(b.port), DB_HOST: PGHOST, DB_PORT: String(PGPORT), DB_NAME: b.db, DB_USER: PGUSER, DB_PASSWORD: PGPASSWORD,
    REDIS_URL: `${REDIS}/${b.redisDb}`, SESSION_SECRET: SECRET, ENCRYPTION_KEY: KEY,
    APP_URL: 'http://localhost:4100', FRONTEND_URL: 'http://localhost:5173',
    NODE_ENV: 'test', E2E_DISABLE_IMAP_CONNECT: 'true', TZ: 'UTC', LOG_LEVEL: 'warn',
    MIGRATIONS_DIR: path.join(root, 'backend/migrations'),
  };
}

async function recreateDatabases() {
  const admin = new pg.Client({ host: PGHOST, port: PGPORT, user: PGUSER, password: PGPASSWORD, database: 'postgres' });
  await admin.connect();
  for (const b of Object.values(BACKENDS)) {
    await admin.query(`DROP DATABASE IF EXISTS ${b.db} WITH (FORCE)`);
    await admin.query(`CREATE DATABASE ${b.db}`);
  }
  await admin.end();
  const r = createClient({ url: REDIS });
  await r.connect();
  for (const b of Object.values(BACKENDS)) { await r.select(b.redisDb); await r.flushDb(); }
  await r.quit();
}

const children = [];
function start(name) {
  const b = BACKENDS[name];
  const child = name === 'node'
    ? spawn('npx', ['tsx', 'src/index.ts'], { cwd: path.join(root, 'backend'), env: env(b), stdio: ['ignore', 'pipe', 'pipe'] })
    : spawn(RUST_BIN, [], { cwd: path.join(root, 'backend-rs'), env: env(b), stdio: ['ignore', 'pipe', 'pipe'] });
  b.log = [];
  for (const s of [child.stdout, child.stderr]) s.on('data', d => { b.log.push(String(d)); if (b.log.length > 400) b.log.shift(); });
  child.on('exit', code => { if (!b.stopping) console.error(`${name} backend exited (${code}):\n${b.log.join('')}`); });
  b.child = child;
  children.push(b);
}

async function waitHealthy(name, timeoutMs = 90_000) {
  const b = BACKENDS[name];
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    try { if ((await fetch(`http://127.0.0.1:${b.port}/api/health`)).ok) return; } catch { /* not up yet */ }
    await new Promise(r => setTimeout(r, 250));
  }
  throw new Error(`${name} backend did not become healthy:\n${b.log.join('')}`);
}

export function stopAll() {
  for (const b of children) { b.stopping = true; b.child.kill('SIGTERM'); }
}

// ── Sessions without the login route: write express-session records directly. ──────────
function signedCookie(id) {
  const sig = createHmac('sha256', SECRET).update(id).digest('base64').replace(/=+$/, '');
  return `connect.sid=${encodeURIComponent(`s:${id}.${sig}`)}`;
}

export async function makeSession(name, data) {
  const b = BACKENDS[name];
  const r = createClient({ url: `${REDIS}/${b.redisDb}` });
  await r.connect();
  const id = `contract${Math.random().toString(36).slice(2)}${Date.now().toString(36)}`.padEnd(32, '0').slice(0, 32);
  const expires = new Date(Date.now() + 7 * 864e5).toISOString();
  await r.set(`sess:${id}`, JSON.stringify({ cookie: { originalMaxAge: 7 * 864e5, expires, secure: false, httpOnly: true, path: '/', sameSite: 'lax' }, ...data }), { EX: 7 * 86400 });
  await r.quit();
  return signedCookie(id);
}

export async function sql(name, text, params = []) {
  const c = new pg.Client({ host: PGHOST, port: PGPORT, user: PGUSER, password: PGPASSWORD, database: BACKENDS[name].db });
  await c.connect();
  try { return await c.query(text, params); } finally { await c.end(); }
}

// ── Normalisation ───────────────────────────────────────────────────────────────────────
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const ISO = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})$/;

export function normaliser() {
  const ids = new Map();
  const fixed = new Set();
  const mapId = (u) => {
    if (fixed.has(u)) return u;
    if (!ids.has(u)) ids.set(u, `<uuid#${ids.size}>`);
    return ids.get(u);
  };
  const norm = (v) => {
    if (typeof v === 'string') {
      if (UUID.test(v)) return mapId(v);
      if (ISO.test(v)) return '<timestamp>';
      // UUIDs embedded in larger strings (vCard UIDs, ids like "<uuid>@<recurrence>").
      return v.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, mapId);
    }
    if (Array.isArray(v)) return v.map(norm);
    if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, norm(x)]));
    return v;
  };
  norm.keep = (...values) => values.forEach(x => fixed.add(x));
  return norm;
}

const COMPARED_HEADERS = ['content-type', 'x-content-type-options', 'x-frame-options', 'referrer-policy', 'access-control-allow-origin', 'access-control-allow-credentials', 'cache-control', 'content-disposition', 'location', 'dav', 'etag'];

async function send(name, step, cookies) {
  const b = BACKENDS[name];
  const headers = { ...(step.headers || {}) };
  const jar = step.as ? cookies[step.as] : undefined;
  if (jar) headers.cookie = jar;
  let body;
  if (step.body !== undefined) {
    if (typeof step.body === 'string') body = step.body;
    else { body = JSON.stringify(step.body); headers['content-type'] ??= 'application/json'; }
  }
  if (!['GET', 'HEAD', 'OPTIONS'].includes(step.method || 'GET') && step.csrf !== false) headers['x-requested-with'] ??= 'XMLHttpRequest';
  const res = await fetch(`http://127.0.0.1:${b.port}${step.path}`, { method: step.method || 'GET', headers, body, redirect: 'manual' });
  const text = await res.text();
  let json;
  try { json = JSON.parse(text); } catch { json = undefined; }
  const setCookie = res.headers.get('set-cookie');
  if (setCookie && step.as) cookies[step.as] = setCookie.split(';')[0];
  const h = {};
  for (const k of step.compareHeaders || COMPARED_HEADERS) {
    let v = res.headers.get(k);
    if (v === null) continue;
    if (k === 'content-type') v = v.replace(/;\s*charset=utf-8/i, '; charset=utf-8');
    if (k === 'etag') v = step.exactEtag ? v : '<present>';
    h[k] = v;
  }
  return { status: res.status, headers: h, json, text: json === undefined ? text : undefined, setsCookie: Boolean(setCookie), bytes: step.compareBytes ? text.length : undefined };
}

function diff(a, b, at = '$') {
  if (JSON.stringify(a) === JSON.stringify(b)) return null;
  if (a && b && typeof a === 'object' && typeof b === 'object' && Array.isArray(a) === Array.isArray(b)) {
    const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
    for (const k of keys) { const d = diff(a[k], b[k], `${at}.${k}`); if (d) return d; }
    if (!Array.isArray(a) && JSON.stringify(Object.keys(a)) !== JSON.stringify(Object.keys(b))) return `${at}: key order ${JSON.stringify(Object.keys(a))} vs ${JSON.stringify(Object.keys(b))}`;
  }
  return `${at}: node=${JSON.stringify(a)?.slice(0, 400)} rust=${JSON.stringify(b)?.slice(0, 400)}`;
}

async function main() {
  const filter = process.argv[2];
  if (!process.env.SKIP_BUILD) execFileSync('cargo', ['build', '-q', '-p', 'inboxora-server'], { cwd: path.join(root, 'backend-rs'), stdio: 'inherit' });
  await recreateDatabases();
  start('node');
  await waitHealthy('node');
  start('rust');
  await waitHealthy('rust');

  // Schema parity: Rust ran its own migration runner on an empty database.
  const dump = (db) => execFileSync(path.join(PGBIN, 'pg_dump'), ['-h', PGHOST, '-p', String(PGPORT), '-U', PGUSER, '--schema-only', '--no-owner', db], { env: { ...process.env, PGPASSWORD }, maxBuffer: 1 << 28 }).toString();
  const strip = (s) => s.replace(/^\\(un)?restrict .*$/gm, '');
  const schemaSame = strip(dump(BACKENDS.node.db)) === strip(dump(BACKENDS.rust.db));
  const migs = async (n) => (await sql(n, 'SELECT version, sha256 FROM schema_migrations ORDER BY version')).rows;
  const migSame = JSON.stringify(await migs('node')) === JSON.stringify(await migs('rust'));
  let failures = 0;
  console.log(`${schemaSame ? 'ok  ' : 'FAIL'} schema: identical pg_dump after migrations`);
  console.log(`${migSame ? 'ok  ' : 'FAIL'} schema_migrations rows identical`);
  if (!schemaSame) failures++;
  if (!migSame) failures++;

  const files = readdirSync(path.join(here, 'scenarios')).filter(f => f.endsWith('.mjs')).sort();
  for (const f of files) {
    const mod = await import(pathToFileURL(path.join(here, 'scenarios', f)));
    for (const scenario of mod.default) {
      if (filter && !`${f}:${scenario.name}`.includes(filter)) continue;
      const ctx = {};
      const cookies = { node: {}, rust: {} };
      const norms = { node: normaliser(), rust: normaliser() };
      try {
        for (const name of ['node', 'rust']) {
          const seeded = scenario.seed ? await scenario.seed({ sql: (t, p) => sql(name, t, p), backend: name }) : {};
          ctx[name] = seeded || {};
          for (const [who, data] of Object.entries(scenario.sessions?.(ctx[name]) || {})) cookies[name][who] = await makeSession(name, data);
          norms[name].keep(...(ctx[name].keep || []));
        }
        let failed = null;
        for (const [i, rawStep] of scenario.steps.entries()) {
          const results = {};
          const waitMs = (typeof rawStep === 'function' ? rawStep(ctx.node) : rawStep).waitMs;
          if (waitMs) await new Promise(r => setTimeout(r, waitMs));
          for (const name of ['node', 'rust']) {
            const step = typeof rawStep === 'function' ? rawStep(ctx[name]) : rawStep;
            const r = await send(name, step, cookies[name]);
            r.json = r.json === undefined ? undefined : norms[name](r.json);
            if (r.text !== undefined) r.text = norms[name](r.text);
            if (step.ignore) for (const p of step.ignore) deletePath(r.json, p);
            if (scenario.ignoreKeys) deleteKeys(r.json, scenario.ignoreKeys);
            results[name] = r;
            if (step.capture && r.json) Object.assign(ctx[name], step.capture(r.json, ctx[name]));
          }
          const step = typeof rawStep === 'function' ? rawStep(ctx.node) : rawStep;
          const fields = ['status', 'headers', 'json', 'text', ...(step.compareSetCookie ? ['setsCookie'] : [])];
          for (const field of fields) {
            const d = diff(results.node[field], results.rust[field], field);
            if (d) { failed = `step ${i} ${step.method || 'GET'} ${step.path}: ${d}`; break; }
          }
          if (!failed && scenario.check) {
            for (const name of ['node', 'rust']) await scenario.check?.[i]?.({ sql: (t, p) => sql(name, t, p), ctx: ctx[name], result: results[name] });
          }
          if (failed) break;
        }
        if (!failed && scenario.after) {
          const a = await scenario.after({ sql: (t, p) => sql('node', t, p), ctx: ctx.node });
          const b = await scenario.after({ sql: (t, p) => sql('rust', t, p), ctx: ctx.rust });
          const d = diff(norms.node(a), norms.rust(b), 'db');
          if (d) failed = `database state: ${d}`;
        }
        console.log(`${failed ? 'FAIL' : 'ok  '} ${f}: ${scenario.name}${failed ? `\n      ${failed}` : ''}`);
        if (failed) failures++;
      } catch (err) {
        failures++;
        console.log(`FAIL ${f}: ${scenario.name}\n      ${err.stack}`);
      }
    }
  }
  stopAll();
  console.log(failures ? `\n${failures} failure(s)` : '\nall contract checks passed');
  process.exit(failures ? 1 : 0);
}

function deleteKeys(obj, keys) {
  if (!obj || typeof obj !== 'object') return;
  for (const k of Object.keys(obj)) {
    if (!Array.isArray(obj) && keys.includes(k)) delete obj[k];
    else deleteKeys(obj[k], keys);
  }
}

function deletePath(obj, p) {
  const parts = p.split('.');
  let cur = obj;
  for (const [i, k] of parts.entries()) {
    if (cur == null) return;
    if (k === '*') { for (const item of Object.values(cur)) deletePath(item, parts.slice(i + 1).join('.')); return; }
    if (i === parts.length - 1) delete cur[k];
    else cur = cur[k];
  }
}

process.on('SIGINT', () => { stopAll(); process.exit(130); });
if (import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(err => { console.error(err); stopAll(); process.exit(1); });
