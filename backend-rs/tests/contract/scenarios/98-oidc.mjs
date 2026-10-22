// OpenID Connect sign-in against a mock IdP: start (PKCE), callback validation, the three
// provisioning modes, account linking, identities and RP-initiated logout.
import { createCipheriv, randomBytes } from 'node:crypto';
import { U } from '../seed-mail.mjs';
import { idp, code } from '../mock-idp.mjs';

const IDP = await idp(2561);
const KEY = Buffer.from('1'.repeat(64), 'hex');
function encrypt(text) {
  const iv = randomBytes(12);
  const c = createCipheriv('aes-256-gcm', KEY, iv);
  const ct = Buffer.concat([c.update(text, 'utf8'), c.final()]);
  return `enc:v1:${iv.toString('hex')}:${c.getAuthTag().toString('hex')}:${ct.toString('hex')}`;
}

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice]);
  await sql(`UPDATE users SET display_name = NULL, is_admin = false WHERE id = $1`, [U.alice]);
  await sql(`DELETE FROM users WHERE username = 'new.user@corp.test'`);
  await sql('DELETE FROM user_identities');
  await sql('DELETE FROM oidc_providers');
  const providers = [
    ['Mock Existing', 'mock-existing', 'login_existing_only', null, encrypt('sec'), true, null, null],
    ['Mock Open', 'mock-open', 'open', 'corp.test, other.test', 'plain-secret', false, 'groups', 'admins'],
    ['Mock Off', 'mock-off', 'disabled', null, encrypt('off-secret'), false, null, null],
  ];
  for (const [name, slug, mode, domains, secret, logout, claim, value] of providers) {
    await sql(`INSERT INTO oidc_providers (name, slug, issuer_url, client_id, client_secret, scopes, provisioning_mode, allowed_domains, enabled, require_email_verified, allow_insecure, admin_group_claim, admin_group_value, rp_initiated_logout, login_match_claim)
      VALUES ($1, $2, $3, 'inboxora', $4, 'openid email', $5, $6, true, true, true, $7, $8, $9, 'email')`, [name, slug, IDP.issuer, secret, mode, domains === 'corp.test, other.test' ? 'corp.test' : domains, claim, value, logout]);
  }
  return { keep: [U.alice] };
}

const HEADERS = ['content-type', 'vary'];
const start = (slug, as, query = '') => ({
  path: `/auth/oidc/${slug}/start${query}`, as, compareHeaders: HEADERS, textPattern: /(?<=(?:state|nonce|code_challenge)=)[A-Za-z0-9_-]+/g,
  capture: (_j, _c, raw) => {
    const u = new URL(raw.location);
    return { state: u.searchParams.get('state'), nonce: u.searchParams.get('nonce'), challenge: u.searchParams.get('code_challenge'), authorize: `${u.origin}${u.pathname}` };
  },
});
const callback = (slug, as, claims, extra = '') => (c) => ({
  path: `/auth/oidc/${slug}/callback?state=${c.state}&code=${code(typeof claims === 'function' ? claims(c) : claims)}${extra}`, as,
});
const verified = (sub, email, more = {}) => (c) => ({ sub, email, email_verified: true, nonce: c.nonce, ...more });

export default [{
  name: 'OIDC start, callback, provisioning, linking and logout',
  seed,
  steps: [
    { path: '/api/auth/oidc/providers' },
    { path: '/auth/oidc/nope/start' },
    { path: '/auth/oidc/mock-existing/start?action=link' },
    start('mock-existing', 'sso1'),
    (c) => ({ path: `/auth/oidc/mock-existing/callback?state=wrong&code=x`, as: 'sso1' }),
    callback('mock-existing', 'sso1', verified('u-alice', 'Alice@Example.test')),
    start('mock-existing', 'sso1'),
    (c) => ({ path: `/auth/oidc/mock-existing/callback?error=access_denied&error_description=Denied%20by%20user&state=${c.state}`, as: 'sso1' }),
    callback('mock-existing', 'sso1', (c) => ({ sub: 'u-alice', email: 'alice@example.test', email_verified: false, nonce: c.nonce })),
    start('mock-existing', 'sso1'),
    callback('mock-existing', 'sso1', { sub: 'u-alice', email: 'alice@example.test', email_verified: true, nonce: 'other' }),
    start('mock-existing', 'sso1'),
    callback('mock-existing', 'sso1', verified('u-alice', 'alice@example.test', { expired: true })),
    start('mock-existing', 'sso1'),
    callback('mock-existing', 'sso1', verified('u-alice', 'alice@example.test', { audience: 'someone-else' })),
    start('mock-existing', 'sso1'),
    (c) => ({ path: `/auth/oidc/mock-existing/callback?state=${c.state}`, as: 'sso1' }),
    start('mock-existing', 'sso1'),
    callback('mock-existing', 'sso1', verified('u-nobody', 'nobody@example.test')),
    start('mock-existing', 'sso1'),
    callback('mock-existing', 'sso1', verified('u-alice', 'Alice@Example.test', { name: '  Alice Example  ' })),
    { path: '/api/auth/me', as: 'sso1' },
    { path: '/api/auth/oidc/identities', as: 'sso1' },
    start('mock-open', 'sso2'),
    callback('mock-open', 'sso2', verified('u-new', 'New.User@corp.test', { name: 'New User', groups: ['staff', 'admins'] })),
    { path: '/api/auth/me', as: 'sso2' },
    start('mock-open', 'sso3'),
    callback('mock-open', 'sso3', verified('u-other', 'x@other.test')),
    start('mock-off', 'sso4'),
    callback('mock-off', 'sso4', verified('u-off', 'off@corp.test')),
    start('mock-off', 'sso1', '?action=link'),
    callback('mock-off', 'sso1', (c) => ({ sub: 'link-1', email: 'alias@corp.test', email_verified: false, nonce: c.nonce })),
    start('mock-open', 'sso1', '?action=link'),
    callback('mock-open', 'sso1', verified('u-new', 'New.User@corp.test')),
    { path: '/api/auth/oidc/identities', as: 'sso1', capture: (j) => ({ linked: j.identities.find(i => i.provider_slug === 'mock-off')?.id }) },
    (c) => ({ method: 'DELETE', path: `/api/auth/oidc/identities/${c.linked}`, as: 'sso1' }),
    { path: '/api/auth/oidc/identities', as: 'sso1' },
    { method: 'DELETE', path: '/api/auth/oidc/identities/00000000-0000-4000-8000-000000000000', as: 'sso2' },
    { method: 'POST', path: '/api/auth/logout', as: 'sso1', body: {}, ignore: ['endSessionUrl'], capture: (j) => ({ end: j.endSessionUrl }) },
    { method: 'POST', path: '/api/auth/logout', as: 'sso2', body: {} },
  ],
  check: {
    20: ({ result }) => { if (result.status !== 302 || !String(result.text).includes('oidc_success=login')) throw new Error(`SSO login must succeed: ${result.status} ${result.text}`); },
    24: ({ result }) => { if (!String(result.text).includes('oidc_success=login')) throw new Error(`open provisioning must succeed: ${result.text}`); },
    31: ({ result }) => { if (!String(result.text).includes('oidc_success=linked')) throw new Error(`linking must succeed: ${result.text}`); },
    38: ({ ctx }) => {
      if (!ctx.end?.startsWith(`${IDP.issuer}/logout?client_id=inboxora&id_token_hint=ey`) || !ctx.end.endsWith('&post_logout_redirect_uri=http%3A%2F%2Flocalhost%3A4100%2Flogin')) throw new Error(`end-session URL: ${ctx.end}`);
    },
  },
  after: async ({ sql }) => ({
    users: (await sql(`SELECT username, display_name, is_admin, password_hash IS NULL AS sso_only FROM users WHERE username IN ('alice@example.test', 'new.user@corp.test') ORDER BY username`)).rows,
    identities: (await sql(`SELECT u.username, op.slug, ui.subject, ui.email, ui.email_verified FROM user_identities ui JOIN users u ON u.id = ui.user_id JOIN oidc_providers op ON op.id = ui.provider_id ORDER BY ui.subject`)).rows,
    books: (await sql(`SELECT count(*)::int AS n FROM address_books b JOIN users u ON u.id = b.user_id WHERE u.username = 'new.user@corp.test'`)).rows,
    events: (await sql(`SELECT event_type, username FROM auth_events WHERE event_type = 'sso_login' ORDER BY created_at`)).rows,
    pkce: IDP.tokenRequests.length > 0,
  }),
}];
