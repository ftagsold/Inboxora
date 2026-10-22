// /carddav, /caldav and the RFC 6764 well-known redirects (DAV application-password auth)
import { createHash } from 'node:crypto';
import { U } from '../seed-mail.mjs';

const BOOK = '00000000-0000-4000-8000-0000000ab001';
const REMOTE_BOOK = '00000000-0000-4000-8000-0000000ab002';
const BOB_BOOK = '00000000-0000-4000-8000-0000000ab003';
const CAL = '00000000-0000-4000-8000-0000000ca101';
const RO_CAL = '00000000-0000-4000-8000-0000000ca102';
const PREFIX = 'mf_dav_00000000-0000-4000-8000-00000000da01';
// bcryptjs hash (cost 4) of the secret below.
const HASH = '$2a$04$.DTOk/9ufpaOXB.cN0LyfuM.YcEc0lfE2EOEqfGQK9ORry8.bFOt.';
const basic = (user, pass) => `Basic ${Buffer.from(`${user}:${pass}`).toString('base64')}`;
const AUTH = { authorization: basic('alice@example.test', `${PREFIX}.Secret_secret_1234`) };
const md5 = (s) => createHash('md5').update(s).digest('hex');
const AUTH_HEADERS = ['content-type', 'www-authenticate', 'retry-after', 'dav', 'allow', 'etag', 'location', 'vary'];

const card = (uid, name, email) => `BEGIN:VCARD\r\nVERSION:3.0\r\nUID:${uid}\r\nFN:${name}\r\nN:${name};;;;\r\nEMAIL;TYPE=INTERNET:${email}\r\nEND:VCARD\r\n`;
const SEEDED = card('seed-1', 'Seeded Person', 'seed@x.test');
const NEW_CARD = card('new-1', 'New Person', 'New@X.test');
const NEW_CARD_2 = card('new-1', 'New Person Renamed', 'new@x.test');
const event = (uid, summary, start, end) => `BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:${uid}\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:${start}\r\nDTEND:${end}\r\nSUMMARY:${summary}\r\nATTENDEE:mailto:bob@x.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n`;
const EVENT_A = event('ev-a', 'Planning', '20261101T090000Z', '20261101T100000Z');
const EVENT_B = event('ev-b', 'Review', '20261201T090000Z', '20261201T100000Z');

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql('DELETE FROM dav_app_passwords WHERE user_id = $1', [U.alice]);
  await sql(`INSERT INTO dav_app_passwords (user_id, label, token_prefix, secret_hash) VALUES ($1, 'Phone', $2, $3)`, [U.alice, PREFIX, HASH]);
  await sql('DELETE FROM contacts WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql('DELETE FROM address_books WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql(`INSERT INTO address_books (id, user_id, name, source, created_at) VALUES ($1, $4, 'Personal', 'local', '2026-01-01'), ($2, $4, 'Remote', 'carddav', '2026-01-02'), ($3, $5, 'Bob', 'local', '2026-01-03')`, [BOOK, REMOTE_BOOK, BOB_BOOK, U.alice, U.bob]);
  await sql(`INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, primary_email) VALUES ($1, $2, 'seed-1', $3, $4, 'Seeded Person', 'seed@x.test')`, [BOOK, U.alice, SEEDED, md5(SEEDED)]);
  await sql(`INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, dav_filename) VALUES ($1, $2, 'remote-1', $3, 'r-etag', 'Remote Person', 'remote 1.vcf')`, [REMOTE_BOOK, U.alice, card('remote-1', 'Remote Person', 'r@x.test')]);
  await sql('DELETE FROM calendars WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql(`INSERT INTO calendars (id, user_id, owner_user_id, name, source, read_only, created_at) VALUES ($1, $3, $3, 'Personal', 'local', false, '2026-01-01'), ($2, $3, $3, 'Holidays', 'local', true, '2026-01-02')`, [CAL, RO_CAL, U.alice]);
  await sql(`INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at) VALUES ($1, $2, 'ev-a', $3, 'etag-a', 'Planning', '2026-11-01T09:00:00Z', '2026-11-01T10:00:00Z')`, [CAL, U.alice, EVENT_A]);
  return { keep: [U.alice, U.bob, BOOK, REMOTE_BOOK, BOB_BOOK, CAL, RO_CAL] };
}

const dav = (method, path, extra = {}) => ({ method, path, csrf: false, ...extra, headers: { ...AUTH, ...(extra.headers || {}) } });
const xml = (body) => ({ body, headers: { 'content-type': 'application/xml' } });
const vcf = (body, headers = {}) => ({ body, headers: { 'content-type': 'text/vcard', ...headers } });
const ics = (body, headers = {}) => ({ body, headers: { 'content-type': 'text/calendar', ...headers } });
const syncReport = (token) => xml(`<?xml version="1.0"?><D:sync-collection xmlns:D="DAV:"><D:sync-token>${token}</D:sync-token><D:prop><D:getetag/></D:prop></D:sync-collection>`);

const scenarios = [{
  name: 'CardDAV server',
  seed,
  steps: [
    { method: 'PROPFIND', path: '/carddav/', compareHeaders: AUTH_HEADERS },
    { method: 'PROPFIND', path: '/carddav/', headers: { authorization: basic('alice@example.test', 'not-an-app-password') }, compareHeaders: AUTH_HEADERS },
    { method: 'PROPFIND', path: '/carddav/', headers: { authorization: 'Basic bm9jb2xvbg==' }, compareHeaders: AUTH_HEADERS },
    dav('PROPFIND', '/carddav/'),
    dav('PROPFIND', '/carddav'),
    { ...dav('OPTIONS', '/carddav/anything/at/all'), compareHeaders: AUTH_HEADERS },
    dav('PROPFIND', `/carddav/${U.alice}/`),
    dav('PROPFIND', `/carddav/${U.alice}`, { headers: { depth: '1' } }),
    dav('PROPFIND', `/carddav/${U.bob}/`),
    dav('PROPFIND', `/carddav/${U.alice}/${BOOK}/`),
    dav('PROPFIND', `/carddav/${U.alice}/${BOOK}/`, { headers: { depth: '1' } }),
    dav('PROPFIND', `/carddav/${U.alice}/${REMOTE_BOOK}`, { headers: { depth: 'infinity' } }),
    dav('PROPFIND', `/carddav/${U.alice}/${BOB_BOOK}/`),
    dav('PROPFIND', `/carddav/${U.alice}/not-a-uuid/`),
    dav('PROPFIND', `/carddav/${U.alice}/%E0/`),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, xml('<C:addressbook-query xmlns:C="urn:ietf:params:xml:ns:carddav"/>')),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, xml('<D:unknown xmlns:D="DAV:"/>')),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, syncReport('')),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, syncReport(`urn:inboxora:carddav:${BOOK}:0`)),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, syncReport(`urn:inboxora:carddav:${BOOK}:999`)),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, syncReport('garbage')),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, xml(`<C:addressbook-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:href>/carddav/${U.alice}/${BOOK}/seed-1.vcf</D:href><D:href>missing%20one.vcf</D:href><D:href>seed-1.vcf</D:href></C:addressbook-multiget>`)),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, xml('<C:addressbook-multiget xmlns:D="DAV:"><D:href>%E0</D:href></C:addressbook-multiget>')),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, xml('<C:addressbook-multiget/>')),
    dav('GET', `/carddav/${U.alice}/${BOOK}/seed-1.vcf`, { exactEtag: true }),
    dav('GET', `/carddav/${U.alice}/${BOOK}/seed-1.vcf`, { headers: { 'if-none-match': `"${md5(SEEDED)}"` } }),
    dav('HEAD', `/carddav/${U.alice}/${BOOK}/seed-1.vcf`),
    dav('GET', `/carddav/${U.alice}/${REMOTE_BOOK}/remote%201.vcf`),
    dav('GET', `/carddav/${U.alice}/${BOOK}/nope.vcf`),
    dav('GET', `/carddav/${U.bob}/${BOB_BOOK}/x.vcf`),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/new-1.vcf`, { ...vcf(NEW_CARD, { 'if-none-match': '*' }), exactEtag: true }),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/new-1.vcf`, vcf(NEW_CARD, { 'if-none-match': '*' })),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/new-1.vcf`, vcf(NEW_CARD_2, { 'if-match': '"wrong"' })),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/other.vcf`, vcf(NEW_CARD_2)),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/new-1.vcf`, { ...vcf(NEW_CARD_2, { 'if-match': `"${md5(NEW_CARD)}"` }), exactEtag: true }),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/ghost.vcf`, vcf(card('ghost', 'Ghost', 'g@x.test'), { 'if-match': '*' })),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/blank.vcf`, vcf('  \r\n')),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/baddate.vcf`, vcf('BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bad\r\nBDAY:not-a-date\r\nEND:VCARD\r\n')),
    dav('PUT', `/carddav/${U.alice}/${BOOK}/NoUid.VCF`, vcf('BEGIN:VCARD\r\nVERSION:3.0\r\nFN:No Uid\r\nEMAIL:nouid@x.test\r\nEND:VCARD\r\n')),
    dav('PUT', `/carddav/${U.alice}/${REMOTE_BOOK}/x.vcf`, vcf(NEW_CARD)),
    dav('PUT', `/carddav/${U.alice}/00000000-0000-4000-8000-000000000000/x.vcf`, vcf(NEW_CARD)),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, syncReport(`urn:inboxora:carddav:${BOOK}:1`)),
    dav('DELETE', `/carddav/${U.alice}/${BOOK}/new-1.vcf`, { headers: { 'if-match': '"wrong"' } }),
    dav('DELETE', `/carddav/${U.alice}/${BOOK}/new-1.vcf`, { headers: { 'if-match': `"${md5(NEW_CARD_2)}"` } }),
    dav('DELETE', `/carddav/${U.alice}/${BOOK}/new-1.vcf`),
    dav('DELETE', `/carddav/${U.alice}/${REMOTE_BOOK}/remote%201.vcf`),
    dav('REPORT', `/carddav/${U.alice}/${BOOK}/`, syncReport(`urn:inboxora:carddav:${BOOK}:0`)),
    dav('MKCOL', `/carddav/${U.alice}/${BOOK}/`),
    dav('PROPFIND', `/carddav/${U.alice}/${BOOK}/a/b`),
    { path: '/.well-known/carddav', compareHeaders: AUTH_HEADERS },
    { path: '/.well-known/carddav', headers: { accept: 'text/html' }, compareHeaders: AUTH_HEADERS },
    { method: 'PROPFIND', path: '/.well-known/carddav', csrf: false, headers: { accept: 'application/json' }, compareHeaders: AUTH_HEADERS },
  ],
  after: async ({ sql }) => ({
    contacts: (await sql(`SELECT address_book_id, uid, dav_filename, etag, display_name, first_name, last_name, primary_email, emails, is_auto, vcard FROM contacts WHERE user_id = $1 ORDER BY uid`, [U.alice])).rows,
    books: (await sql('SELECT id, sync_version FROM address_books WHERE user_id = $1 ORDER BY id', [U.alice])).rows,
    changes: (await sql('SELECT filename, version, etag, deleted FROM contact_sync_changes WHERE address_book_id = $1 ORDER BY version, filename', [BOOK])).rows,
    used: (await sql('SELECT last_used_at IS NOT NULL AS used FROM dav_app_passwords WHERE user_id = $1', [U.alice])).rows,
    failures: (await sql(`SELECT event_type, username, success FROM auth_events WHERE event_type = 'carddav_auth_fail' ORDER BY created_at, username NULLS FIRST`)).rows,
  }),
}, {
  name: 'CalDAV server',
  seed,
  steps: [
    { method: 'PROPFIND', path: '/caldav/', compareHeaders: AUTH_HEADERS },
    dav('PROPFIND', '/caldav/'),
    { ...dav('OPTIONS', '/caldav/'), compareHeaders: AUTH_HEADERS },
    dav('PROPFIND', `/caldav/${U.alice}/`),
    dav('PROPFIND', `/caldav/${U.bob}/`),
    dav('PROPFIND', `/caldav/${U.alice}/${CAL}/`),
    dav('PROPFIND', `/caldav/${U.alice}/${BOOK}/`),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav"/>')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:calendar-query><C:time-range start="20261201T000000Z" end="20261231T000000Z"/></C:calendar-query>')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:calendar-query><C:TIME-RANGE START="20261001T000000Z" END="20261130T000000Z"/></C:calendar-query>')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:calendar-query><C:time-range start="20261231T000000Z" end="20261201T000000Z"/></C:calendar-query>')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:calendar-query><C:time-range start="20261341T000000Z" end="20261401T000000Z"/></C:calendar-query>')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:nothing/>')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, syncReport('')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, syncReport('sync-0')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, syncReport('sync-999')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, syncReport('bogus')),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml(`<C:calendar-multiget xmlns:D="DAV:"><D:href>/caldav/${U.alice}/${CAL}/ev-a.ics</D:href><D:href>/x/</D:href><D:href>missing.ics</D:href></C:calendar-multiget>`)),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, xml('<C:calendar-multiget xmlns:D="DAV:"><D:href>%E0</D:href></C:calendar-multiget>')),
    dav('GET', `/caldav/${U.alice}/${CAL}/ev-a.ics`, { exactEtag: true }),
    dav('GET', `/caldav/${U.alice}/${CAL}/ev-a.ics`, { headers: { 'if-none-match': '"etag-a"' } }),
    dav('GET', `/caldav/${U.alice}/${CAL}/missing.ics`),
    dav('PUT', `/caldav/${U.alice}/${CAL}/ev-b.ics`, ics(EVENT_B, { 'if-none-match': '*' })),
    dav('PUT', `/caldav/${U.alice}/${CAL}/ev-b.ics`, ics(EVENT_B, { 'if-none-match': '*' })),
    dav('PUT', `/caldav/${U.alice}/${CAL}/ev-a.ics`, ics(EVENT_A.replace('Planning', 'Planning v2'), { 'if-match': 'W/"nope", "etag-a"' })),
    dav('PUT', `/caldav/${U.alice}/${CAL}/ev-a.ics`, ics(EVENT_A, { 'if-match': '"etag-a"' })),
    dav('PUT', `/caldav/${U.alice}/${CAL}/renamed.ics`, ics(EVENT_A)),
    dav('PUT', `/caldav/${U.alice}/${CAL}/junk.ics`, ics('not a calendar')),
    dav('PUT', `/caldav/${U.alice}/${RO_CAL}/x.ics`, ics(EVENT_B)),
    dav('PUT', `/caldav/${U.alice}/${BOOK}/x.ics`, ics(EVENT_B)),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, syncReport('sync-1')),
    dav('DELETE', `/caldav/${U.alice}/${CAL}/ev-b.ics`, { headers: { 'if-match': '"nope"' } }),
    dav('DELETE', `/caldav/${U.alice}/${CAL}/ev-b.ics`, { headers: { 'if-match': '*' } }),
    dav('DELETE', `/caldav/${U.alice}/${CAL}/ev-b.ics`),
    dav('DELETE', `/caldav/${U.alice}/${RO_CAL}/ev-b.ics`),
    dav('REPORT', `/caldav/${U.alice}/${CAL}/`, syncReport('sync-0')),
    { path: '/.well-known/caldav', compareHeaders: AUTH_HEADERS },
    { method: 'HEAD', path: '/.well-known/caldav/', compareHeaders: AUTH_HEADERS },
  ],
  after: async ({ sql }) => ({
    events: (await sql(`SELECT calendar_id, uid, dav_filename, summary, starts_at, ends_at, all_day, attendees, raw_ical FROM calendar_events WHERE user_id = $1 ORDER BY uid`, [U.alice])).rows,
    calendars: (await sql('SELECT id, sync_version, sync_token FROM calendars WHERE user_id = $1 ORDER BY id', [U.alice])).rows,
    changes: (await sql('SELECT uid, version, deleted, dav_filename FROM calendar_sync_changes WHERE calendar_id = $1 ORDER BY version, uid', [CAL])).rows,
  }),
}];

// /api/dav-credentials: issue an app password, use it, revoke it.
export const credentials = {
  name: 'DAV application passwords: issue, authenticate, revoke',
  seed: async ({ sql }) => {
    await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
    await sql('DELETE FROM dav_app_passwords WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
    return { keep: [U.alice, U.bob] };
  },
  sessions: () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } }),
  ignoreKeys: ['secret'],
  check: {
    7: ({ result }) => { if (result.status !== 207) throw new Error(`issued password must authenticate, got ${result.status}`); },
    8: ({ result }) => { if (result.status !== 401) throw new Error(`another user's name must not authenticate, got ${result.status}`); },
    13: ({ result }) => { if (result.status !== 401) throw new Error(`revoked password must fail, got ${result.status}`); },
  },
  steps: [
    { path: '/api/dav-credentials' },
    { path: '/api/dav-credentials', as: 'alice' },
    { method: 'POST', path: '/api/dav-credentials', as: 'alice', body: { label: '   ' } },
    { method: 'POST', path: '/api/dav-credentials', as: 'alice', body: { label: 'x'.repeat(121) } },
    { method: 'POST', path: '/api/dav-credentials', as: 'alice', body: { label: 42 } },
    { method: 'POST', path: '/api/dav-credentials', as: 'alice', body: { label: '  Phone  ' }, capture: (j) => ({ secret: j.secret, id: j.credential?.id }) },
    { path: '/api/dav-credentials', as: 'alice' },
    (c) => ({ method: 'PROPFIND', path: `/carddav/${U.alice}/`, csrf: false, headers: { authorization: basic('alice@example.test', c.secret), depth: '1' } }),
    (c) => ({ method: 'PROPFIND', path: '/caldav/', csrf: false, headers: { authorization: basic('bob@example.test', c.secret) } }),
    (c) => ({ method: 'DELETE', path: `/api/dav-credentials/${c.id}`, as: 'bob' }),
    { method: 'DELETE', path: '/api/dav-credentials/not-a-uuid', as: 'alice' },
    (c) => ({ method: 'DELETE', path: `/api/dav-credentials/${c.id}`, as: 'alice' }),
    (c) => ({ method: 'DELETE', path: `/api/dav-credentials/${c.id}`, as: 'alice' }),
    (c) => ({ method: 'PROPFIND', path: '/caldav/', csrf: false, headers: { authorization: basic('alice@example.test', c.secret) } }),
    { path: '/api/dav-credentials', as: 'alice' },
  ],
  after: async ({ sql }) => ({
    passwords: (await sql('SELECT label, revoked_at IS NOT NULL AS revoked, last_used_at IS NOT NULL AS used FROM dav_app_passwords WHERE user_id = $1', [U.alice])).rows,
    books: (await sql('SELECT name, source FROM address_books WHERE user_id = $1 ORDER BY name', [U.alice])).rows,
    calendars: (await sql('SELECT name, source FROM calendars WHERE user_id = $1 ORDER BY name', [U.alice])).rows,
  }),
};

export default [...scenarios, credentials];
