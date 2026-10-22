// Middleware stack and unauthenticated probes.
const USER = '00000000-0000-4000-8000-000000000001';

async function seedUser({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash, is_admin) VALUES ($1, 'locked@example.test', 'x', false) ON CONFLICT DO NOTHING`, [USER]);
  return { keep: [USER] };
}

export default [
  { name: 'health and version', steps: [{ path: '/api/health', exactEtag: true }, { path: '/api/version', ignore: ['version'] }] },
  { name: 'conditional GET answers 304', steps: [
    { path: '/api/health', headers: { 'if-none-match': 'W/"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg"', 'cache-control': 'max-age=0' } },
    { path: '/api/health', headers: { 'if-none-match': 'W/"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg"' } },
  ] },
  { name: 'unknown route is the Express 404 page', steps: [{ path: '/api/definitely-missing' }, { path: '/nope', method: 'GET' }] },
  { name: 'CORS preflight ends with 204', steps: [{ method: 'OPTIONS', path: '/api/mail/messages', headers: { origin: 'http://localhost:5173', 'access-control-request-method': 'POST', 'access-control-request-headers': 'content-type,x-requested-with' }, compareHeaders: ['access-control-allow-origin', 'access-control-allow-credentials', 'access-control-allow-methods', 'access-control-allow-headers', 'vary'] }] },
  { name: 'CSRF header required for mutating /api requests', steps: [{ method: 'POST', path: '/api/health', csrf: false, body: {} }, { method: 'DELETE', path: '/api/whatever', csrf: false }] },
  {
    name: 'locked session only reaches the lock allow-list',
    seed: seedUser,
    sessions: () => ({ alice: { userId: USER, locked: true } }),
    steps: [
      { path: '/api/health', as: 'alice' },
      { path: '/api/contacts', as: 'alice' },
      { path: '/api/mail/accounts?x=1', as: 'alice' },
    ],
  },
];
