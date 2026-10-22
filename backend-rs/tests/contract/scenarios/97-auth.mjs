// /api/auth and /api/totp: invitations, password sign-in, screen lock, profile, preferences,
// password reset (over SMTP), authenticator enrolment and challenge, email OTP enforcement.
import { createHmac } from 'node:crypto';
import bcrypt from 'bcryptjs';
import { U } from '../seed-mail.mjs';
import { sink } from '../smtp-sink.mjs';

const SINK = { node: sink(2551), rust: sink(2552) };
const DAVE_HASH = bcrypt.hashSync('dave-pass-1', 4);
let ipCounter = 0;
// A fresh client address per request, so the shared per-IP auth budget never interferes.
const ip = () => ({ 'x-forwarded-for': `198.51.100.${(ipCounter++ % 250) + 1}` });

function totp(secret, at = Date.now()) {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = '';
  for (const c of secret.replace(/=+$/, '').toUpperCase()) bits += alphabet.indexOf(c).toString(2).padStart(5, '0');
  const key = Buffer.from(bits.match(/.{8}/g).map(b => parseInt(b, 2)));
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(at / 30000)));
  const h = createHmac('sha1', key).update(counter).digest();
  const o = h[19] & 15;
  return String(((h.readUInt32BE(o) & 0x7fffffff) % 1000000)).padStart(6, '0');
}

async function seed({ sql, backend }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice]);
  await sql(`DELETE FROM users WHERE username IN ('carol@example.test', 'dave@example.test')`);
  await sql(`INSERT INTO users (username, password_hash, recovery_email) VALUES ('dave@example.test', $1, 'dave@mail.test')`, [DAVE_HASH]);
  await sql(`DELETE FROM invites`);
  await sql(`INSERT INTO invites (email, token, created_by, expires_at) VALUES ('Carol@Mail.test', 'inv-carol', $1, NOW() + INTERVAL '1 day'), ('x@mail.test', 'inv-old', $1, NOW() - INTERVAL '1 day')`, [U.alice]);
  const settings = {
    registration_open: 'false', mfa_enforcement: 'off', mfa_device_trust: '30d', allow_private_hosts: 'true',
    system_email_config: JSON.stringify({ host: '127.0.0.1', port: SINK[backend].port, user: 'sys', pass: 'pw', fromName: 'Inboxora', fromEmail: 'noreply@inboxora.test' }),
  };
  for (const [key, value] of Object.entries(settings)) {
    await sql(`INSERT INTO system_settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = $2`, [key, value]);
  }
  await sql(`DELETE FROM system_settings WHERE key IN ('internal_auth_disabled', 'custom_css')`);
  return { keep: [U.alice], sink: backend };
}

const post = (path, body, as, extra = {}) => ({ method: 'POST', path: `/api/auth${path}`, body, ...(as ? { as } : {}), headers: ip(), ...extra });
const get = (path, as) => ({ path: `/api/auth${path}`, ...(as ? { as } : {}) });
const latest = (c, re) => SINK[c.sink].lastText().match(re)?.[1];
const PNG = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==';

export default [{
  name: 'password sign-in, lock, profile, preferences, reset, TOTP and email OTP',
  seed,
  ignoreKeys: ['secret', 'qrCode'],
  steps: [
    get('/registration-status'),
    post('/register', {}),
    post('/register', { username: 'bad\u0001name', password: 'x' }),
    post('/register', { username: ' Carol@Example.TEST ', password: 'pw-carol-1' }),
    post('/register', { username: 'carol@example.test', password: 'pw-carol-1', inviteToken: 'inv-old' }),
    post('/register', { username: ' Carol@Example.TEST ', password: 'pw-carol-1', inviteToken: 'inv-carol' }, 'carol'),
    post('/register', { username: 'carol@example.test', password: 'pw-carol-1', inviteToken: 'inv-carol' }),
    get('/me', 'carol'),
    get('/me'),
    get('/invite/inv-carol'),
    post('/login', { username: 'carol@example.test', password: 'wrong' }),
    post('/login', { username: 'carol@example.test' }),
    post('/login', { username: 'nobody@example.test', password: 'x' }),
    post('/login', { username: 7, password: 'x' }),
    post('/login', { username: 'CAROL@example.test ', password: 'pw-carol-1' }, 'carol2'),
    post('/lock', {}, 'carol2'),
    { path: '/api/block-list', as: 'carol2' },
    get('/me', 'carol2'),
    post('/unlock', {}, 'carol2'),
    post('/unlock', { pin: '1234' }, 'carol2'),
    post('/lock-pin', { pin: '12a' }, 'carol'),
    post('/lock-pin', { pin: '1234' }, 'carol'),
    post('/lock-pin', { pin: '5678' }, 'carol'),
    post('/unlock', { pin: '9999' }, 'carol2'),
    post('/unlock', { pin: 1234 }, 'carol2'),
    get('/me', 'carol2'),
    { method: 'PATCH', path: '/api/auth/profile', as: 'carol', body: {} },
    { method: 'PATCH', path: '/api/auth/profile', as: 'carol', body: { displayName: `  ${'C'.repeat(120)}  ` } },
    { method: 'PATCH', path: '/api/auth/profile', as: 'carol', body: { displayName: '  Carol C  ' } },
    post('/avatar', { avatar: 'data:image/svg+xml;base64,PHN2Zz4=' }, 'carol'),
    post('/avatar', { avatar: PNG }, 'carol'),
    get('/me', 'carol'),
    { method: 'DELETE', path: '/api/auth/avatar', as: 'carol' },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: { themeMode: 'neon' } },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: { calendarWorkDays: [1, 1] } },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: { calendarWorkHoursStart: '18:00' } },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: { senderFavicons: 'yes' } },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: { calendarInviteAccountId: '00000000-0000-4000-8000-000000000000' } },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: {
      theme: 'dark', pageSize: 50, blockRemoteImages: true, imageWhitelist: { domains: ['x.test'] }, sidebarWidth: '250px', markReadBehavior: 'delay',
      markReadDelay: 3, autoLockMinutes: '15', folderSyncInterval: 900, aiActions: [{ id: 'sum', label: 'Summarize', prompt: 'Summarize this' }, { id: '', label: 'x', prompt: 'y' }],
      rightSidebarWidth: 300, rightSidebarHidden: false, themeMode: 'dark', themeLight: 'paper', themeDark: 'night_1', calendarWorkDays: [1, 2, 3, 4, 5],
      calendarWorkHoursStart: '08:30', calendarWorkHoursEnd: '16:00', visibleCalendarIds: ['a', 'b', 'a'], calendarWeekStartsOn: 1, mobileNavigationPosition: 'bottom',
      gtdCollapsedSections: { inbox: 1, today: 0 }, gtdPetSlug: ' Cat-1 ', senderFavicons: true, conversationNewestFirst: false, fontSize: 14, replyDefault: 'replyAll',
    } },
    { method: 'PATCH', path: '/api/auth/preferences', as: 'carol', body: { calendarWorkHoursEnd: '20:00', autoLockMinutes: null } },
    get('/preferences', 'carol'),
    post('/preferences/whitelist-add', { type: 'domain', value: ' .Images.Example.COM ' }, 'carol'),
    post('/preferences/whitelist-add', { type: 'address', value: 'not-an-email' }, 'carol'),
    post('/preferences/whitelist-add', { type: 'address', value: 'Pics@Example.com' }, 'carol'),
    get('/preferences', 'carol'),
    { method: 'PATCH', path: '/api/auth/profile/recovery-email', as: 'carol', body: { email: 'nope' } },
    { method: 'PATCH', path: '/api/auth/profile/recovery-email', as: 'carol', body: { email: ' Carol@Mail.TEST ' } },
    get('/profile/recovery-email', 'carol'),
    post('/forgot-password', { email: 'bad' }),
    post('/forgot-password', { email: 'unknown@mail.test' }),
    post('/forgot-password', { email: ' carol@mail.test ' }, undefined, { capture: (j, c) => ({ reset: latest(c, /reset_token=([0-9a-f]{64})/) }) }),
    post('/reset-password', { token: 'nope', password: 'new-pass-123' }),
    (c) => post('/reset-password', { token: c.reset, password: 'short' }),
    (c) => post('/reset-password', { token: ` ${c.reset} `, password: 'new-pass-123' }),
    (c) => post('/reset-password', { token: c.reset, password: 'new-pass-123' }),
    get('/me', 'carol'),
    post('/login', { username: 'carol@example.test', password: 'new-pass-123' }, 'carol3'),
    { path: '/api/totp/setup', as: 'carol3', capture: (j) => ({ secret: j.secret }) },
    { method: 'POST', path: '/api/totp/enable', as: 'carol3', body: { code: '000000' } },
    (c) => ({ method: 'POST', path: '/api/totp/enable', as: 'carol3', body: { code: totp(c.secret) } }),
    { path: '/api/totp/setup', as: 'carol3' },
    get('/me', 'carol3'),
    post('/logout', {}, 'carol3'),
    get('/me', 'carol3'),
    post('/login', { username: 'carol@example.test', password: 'new-pass-123' }, 'carol4'),
    post('/2fa/challenge', {}, 'carol4'),
    post('/2fa/challenge', { code: '000000' }, 'carol4'),
    (c) => post('/2fa/challenge', { code: ` ${totp(c.secret).slice(0, 3)} ${totp(c.secret).slice(3)} ` }, 'carol4'),
    get('/me', 'carol4'),
    { method: 'POST', path: '/api/totp/disable', as: 'carol4', body: { password: 'wrong' } },
    { method: 'POST', path: '/api/totp/disable', as: 'carol4', body: { password: 'new-pass-123' } },
    post('/login', { username: 'dave@example.test', password: 'dave-pass-1' }, 'dave', { capture: (j, c) => ({ otp: latest(c, /sign-in code is: (\d{6})/) }) }),
    post('/2fa/send-email-otp', {}, 'dave', { capture: (j, c) => ({ otp2: latest(c, /sign-in code is: (\d{6})/) }) }),
    post('/2fa/verify-email-otp', { code: '000000' }, 'dave'),
    (c) => post('/2fa/verify-email-otp', { code: c.otp }, 'dave'),
    (c) => post('/2fa/verify-email-otp', { code: ` ${c.otp2} ` }, 'dave'),
    get('/me', 'dave'),
    post('/logout', {}, 'dave'),
  ],
  check: {
    // Enforce MFA only for the last part, so the earlier sign-ins stay single-factor.
    70: async ({ sql }) => { await sql(`UPDATE system_settings SET value = 'required' WHERE key = 'mfa_enforcement'`); },
    5: ({ result }) => { if (result.status !== 200) throw new Error(`invited registration must succeed: ${JSON.stringify(result.json)}`); },
    50: ({ ctx }) => { if (!ctx.reset) throw new Error('the reset email must carry a token'); },
    53: ({ result }) => { if (result.status !== 200) throw new Error(`reset with the mailed token must succeed: ${JSON.stringify(result.json)}`); },
    67: ({ result }) => { if (result.status !== 200) throw new Error(`TOTP challenge must succeed: ${JSON.stringify(result.json)}`); },
    75: ({ result }) => { if (result.status !== 200) throw new Error(`email OTP must succeed: ${JSON.stringify(result.json)}`); },
  },
  after: async ({ sql }) => ({
    users: (await sql(`SELECT username, display_name, avatar IS NOT NULL AS has_avatar, is_admin, totp_enabled, totp_secret IS NULL AS no_secret, recovery_email, lock_pin_hash IS NOT NULL AS has_pin, preferences FROM users WHERE username IN ('carol@example.test', 'dave@example.test') ORDER BY username`)).rows,
    invites: (await sql(`SELECT token, used_at IS NOT NULL AS used FROM invites ORDER BY token`)).rows,
    events: (await sql(`SELECT event_type, username, success FROM auth_events WHERE username IN ('carol@example.test', 'dave@example.test', 'nobody@example.test') ORDER BY created_at, event_type`)).rows,
    otps: (await sql(`SELECT used_at IS NOT NULL AS used FROM email_otp_tokens ORDER BY created_at`)).rows,
  }),
}];
