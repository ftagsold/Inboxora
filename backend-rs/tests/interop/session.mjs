// Two-way session interop between express-session/connect-redis (Node) and inboxora-core.
// Run from backend/: node ../backend-rs/tests/interop/session.mjs
// Needs TEST_REDIS_URL, SESSION_SECRET and a built `session_probe` example.
import express from 'express';
import session from 'express-session';
import RedisStore from 'connect-redis';
import { createClient } from 'redis';
import { execFileSync } from 'child_process';
import assert from 'assert/strict';

const probe = (...args) => execFileSync(process.env.PROBE, args, { encoding: 'utf8' }).trim();
const redis = createClient({ url: process.env.TEST_REDIS_URL });
await redis.connect();
const app = express();
app.set('trust proxy', 1);
app.use(session({
  store: Reflect.construct(RedisStore, [{ client: redis }]),
  secret: process.env.SESSION_SECRET, resave: false, saveUninitialized: false,
  cookie: { secure: 'auto', httpOnly: true, sameSite: 'lax', maxAge: 7 * 24 * 60 * 60 * 1000 },
}));
app.get('/login', (req, res) => { req.session.userId = 'u-node'; req.session.locked = false; res.json({ ok: true }); });
app.get('/me', (req, res) => res.json({ userId: req.session.userId ?? null, extra: req.session.extra ?? null }));
app.get('/noop', (req, res) => res.json({}));
const server = app.listen(0);
const base = `http://127.0.0.1:${server.address().port}`;
const cookieOf = (setCookie) => setCookie.split(';')[0];

// Node → Rust
const r1 = await fetch(`${base}/login`);
const nodeSetCookie = r1.headers.get('set-cookie');
const nodeCookie = cookieOf(nodeSetCookie);
assert.deepEqual(JSON.parse(probe('load', nodeCookie)), { userId: 'u-node', locked: false });

// Rust → Node, including the Set-Cookie format
const rustSetCookie = probe('create', JSON.stringify({ userId: 'u-rust', isAdmin: true }));
const attrs = (s) => s.split('; ').slice(1).map(a => a.split('=')[0]);
assert.deepEqual(attrs(rustSetCookie), attrs(nodeSetCookie), 'cookie attributes and order');
const r2 = await fetch(`${base}/me`, { headers: { cookie: cookieOf(rustSetCookie) } });
assert.deepEqual(await r2.json(), { userId: 'u-rust', extra: null });
assert.equal(r2.headers.get('set-cookie'), null, 'unmodified session sets no cookie (Node)');

// Rust modifies a Node session; Node sees the change. Unmodified Rust commit sets no cookie.
const touched = probe('touch', nodeCookie, 'extra', 'from-rust');
assert.ok(touched.startsWith('connect.sid='));
const r3 = await fetch(`${base}/me`, { headers: { cookie: nodeCookie } });
assert.deepEqual(await r3.json(), { userId: 'u-node', extra: 'from-rust' });

// Stored JSON shape is identical apart from values.
const sid = (c) => decodeURIComponent(c.split('=')[1]).slice(2).split('.')[0];
const nodeRaw = JSON.parse(await redis.get(`sess:${sid(nodeCookie)}`));
const rustRaw = JSON.parse(await redis.get(`sess:${sid(cookieOf(rustSetCookie))}`));
assert.deepEqual(Object.keys(rustRaw.cookie), Object.keys(nodeRaw.cookie));
assert.equal(Object.keys(rustRaw)[0], 'cookie');
assert.ok((await redis.ttl(`sess:${sid(cookieOf(rustSetCookie))}`)) > 7 * 24 * 3600 - 10);

// A tampered signature is rejected by both.
const tampered = cookieOf(rustSetCookie).replace(/.$/, c => (c === 'A' ? 'B' : 'A'));
assert.equal(probe('load', tampered), 'null');
const r4 = await fetch(`${base}/me`, { headers: { cookie: tampered } });
assert.deepEqual(await r4.json(), { userId: null, extra: null });

console.log('session interop: ok');
server.close();
await redis.quit();
