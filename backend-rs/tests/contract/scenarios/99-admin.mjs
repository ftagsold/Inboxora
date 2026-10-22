// /api/admin: gate, users, settings, invites (mailed through the SMTP sink), system email,
// OIDC providers and lock-out protection.
import { U } from '../seed-mail.mjs';
import { sink } from '../smtp-sink.mjs';

const SINK = { node: sink(2553), rust: sink(2554) };
const CAROL = '00000000-0000-4000-8000-0000000000c1';

async function seed({ sql, backend }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`UPDATE users SET is_admin = true, totp_enabled = false WHERE id = $1`, [U.alice]);
  await sql(`UPDATE users SET is_admin = false, totp_enabled = true, totp_secret = 'enc:v1:x' WHERE id = $1`, [U.bob]);
  await sql(`DELETE FROM users WHERE username = 'victim@example.test' OR id = $1`, [CAROL]);
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'victim@example.test', 'x')`, [CAROL]);
  await sql(`DELETE FROM invites`);
  await sql(`DELETE FROM user_identities`);
  await sql(`DELETE FROM oidc_providers`);
  await sql(`DELETE FROM system_settings WHERE key IN ('system_email_config', 'internal_auth_disabled', 'custom_css')`);
  await sql(`INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'false') ON CONFLICT (key) DO UPDATE SET value = 'false'`);
  return { keep: [U.alice, U.bob, CAROL], sinkPort: SINK[backend].port, backend };
}

const sessions = () => ({ admin: { userId: U.alice, username: 'alice@example.test' }, user: { userId: U.bob } });
const a = (method, path, body, extra = {}) => ({ method, path: `/api/admin${path}`, as: 'admin', ...(body !== undefined ? { body } : {}), ...extra });

export default [{
  name: 'admin users, settings, invites, system email and OIDC providers',
  seed,
  sessions,
  ignoreKeys: ['token', 'inviteUrl'],
  steps: [
    { path: '/api/admin/users' },
    { path: '/api/admin/users', as: 'user' },
    { path: '/api/admin/nonexistent', as: 'user' },
    { path: '/api/admin/nonexistent', as: 'admin' },
    a('GET', '/users?limit=2&offset=-5'),
    a('POST', `/users/${U.alice}/totp/disable`, {}),
    a('POST', '/users/not-a-uuid/totp/disable', {}),
    a('POST', '/users/00000000-0000-4000-8000-000000000000/totp/disable', {}),
    a('POST', `/users/${U.bob}/totp/disable`, {}),
    a('PATCH', `/users/${U.alice}`, { isAdmin: false }),
    a('PATCH', `/users/${U.bob}`, { isAdmin: true }),
    a('PATCH', `/users/${U.bob}`, { isAdmin: false }),
    a('DELETE', `/users/${U.alice}`),
    a('DELETE', `/users/${CAROL}`),
    a('DELETE', `/users/${CAROL}`),
    a('GET', '/settings'),
    a('PATCH', '/settings', { auth_max_attempts: 0 }),
    a('PATCH', '/settings', { auth_window_minutes: '2000' }),
    a('PATCH', '/settings', { mfa_enforcement: 'sometimes' }),
    a('PATCH', '/settings', { mfa_device_trust: 'forever' }),
    a('PATCH', '/settings', { custom_css: 7 }),
    a('PATCH', '/settings', { internal_auth_disabled: true }),
    a('PATCH', '/settings', { registration_open: true, auth_max_attempts: '50', auth_window_minutes: 30, allow_private_hosts: true, mfa_device_trust: '7d', custom_css: 'body{color:red}\u0000' }),
    a('GET', '/settings'),
    // Both backends' DAV clients use the Node DAV server in earlier scenarios, so Node's
    // auth_events also hold the Rust run's failures; only the shape is compared here.
    a('GET', '/auth-events?limit=1', undefined, { ignore: ['events', 'total'] }),
    a('GET', '/system-email'),
    a('POST', '/system-email/test', {}),
    a('POST', '/system-email', { host: 'mail.test' }),
    a('POST', '/system-email', { host: '10.0.0.1', user: 'x' }),
    (c) => a('POST', '/system-email', { host: ' 127.0.0.1 ', port: String(c.sinkPort), user: ' sys ', pass: 'pw', fromName: ' Admin Mailer ' }),
    a('GET', '/system-email', undefined, { ignore: ['config.port'] }),
    (c) => a('POST', '/system-email', { host: '127.0.0.1', port: c.sinkPort, user: 'sys', pass: '••••••••', fromEmail: 'admin@inboxora.test' }),
    a('POST', '/system-email/test', {}),
    a('POST', '/invites', { email: 'nope' }),
    a('POST', '/invites', { email: ' Friend@Mail.TEST ' }, { capture: (j, c) => ({ mailed: SINK[c.backend].lastText().includes('/register?invite='), to: SINK[c.backend].messages.at(-1)?.to?.[0] }) }),
    a('GET', '/invites', undefined, { capture: (j) => ({ invite: j.invites[0]?.id }) }),
    (c) => a('DELETE', `/invites/${c.invite}`),
    a('DELETE', '/invites/bad'),
    a('GET', '/invites'),
    a('DELETE', '/system-email'),
    a('GET', '/system-email'),
    a('GET', '/oidc'),
    a('POST', '/oidc', { name: 'X' }),
    a('POST', '/oidc', { name: 'X', slug: 'Bad Slug', issuer_url: 'https://idp.test', client_id: 'c', client_secret: 's' }),
    a('POST', '/oidc', { name: 'X', slug: 'x', issuer_url: 'http://idp.test', client_id: 'c', client_secret: 's' }),
    a('POST', '/oidc', { name: 'X', slug: 'x', issuer_url: 'not a url', client_id: 'c', client_secret: 's' }),
    a('POST', '/oidc', { name: 'X', slug: 'x', issuer_url: 'https://idp.test', client_id: 'c', client_secret: 's', login_match_claim: '__proto__' }),
    a('POST', '/oidc', { name: ' Corp SSO ', slug: 'corp', issuer_url: ' http://127.0.0.1:9/ ', client_id: ' cid ', client_secret: 'sec', allow_insecure: true, allowed_domains: ' corp.test ', admin_group_claim: 'groups', admin_group_value: 'admins', login_match_claim: 'preferred_username' }, { capture: (j) => ({ corp: j.provider?.id }) }),
    a('POST', '/oidc', { name: 'Dup', slug: 'corp', issuer_url: 'http://127.0.0.1:9/', client_id: 'c', client_secret: 's', allow_insecure: true }),
    (c) => a('PATCH', `/oidc/${c.corp}`, { slug: 'UPPER' }),
    (c) => a('PATCH', `/oidc/${c.corp}`, { issuer_url: 'http://127.0.0.1:9/', allow_insecure: false }),
    (c) => a('PATCH', `/oidc/${c.corp}`, { name: 'Corp SSO 2', allowed_domains: '', admin_group_value: null, client_secret: '••••••••', rp_initiated_logout: true }),
    a('PATCH', '/oidc/00000000-0000-4000-8000-000000000000', { name: 'x' }),
    a('PATCH', '/settings', { internal_auth_disabled: true }),
    (c) => a('DELETE', `/oidc/${c.corp}`),
    a('GET', '/oidc'),
  ],
  check: {
    34: ({ ctx }) => { if (!ctx.mailed || ctx.to?.toLowerCase() !== 'friend@mail.test') throw new Error(`the invite must be mailed: ${JSON.stringify(ctx)}`); },
    32: ({ result }) => { if (result.status !== 200) throw new Error(`SMTP verify against the sink must pass: ${JSON.stringify(result.json)}`); },
  },
  after: async ({ sql }) => ({
    users: (await sql(`SELECT username, is_admin, totp_enabled, totp_secret IS NULL AS no_secret FROM users WHERE id IN ($1, $2, $3) ORDER BY username`, [U.alice, U.bob, CAROL])).rows,
    settings: (await sql(`SELECT key, value FROM system_settings WHERE key IN ('registration_open', 'auth_max_attempts', 'auth_window_minutes', 'allow_private_hosts', 'mfa_device_trust', 'custom_css', 'internal_auth_disabled') ORDER BY key`)).rows,
    providers: (await sql(`SELECT name, slug, issuer_url, client_id, client_secret LIKE 'enc:v1:%' AS encrypted, allowed_domains, admin_group_claim, admin_group_value, rp_initiated_logout, login_match_claim FROM oidc_providers ORDER BY slug`)).rows,
  }),
}];
