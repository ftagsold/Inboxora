// /api/dav/servers: discovery, import and sync against a real DAV server. Both backends use the
// Node backend's own /carddav and /caldav as the remote, so discovered hrefs are identical.
import { U } from '../seed-mail.mjs';
import { BACKENDS } from '../harness.mjs';

const REMOTE = `http://127.0.0.1:${BACKENDS.node.port}`;
const BOOK = '00000000-0000-4000-8000-0000000ab101';
const CAL = '00000000-0000-4000-8000-0000000ca201';
const CAL2 = '00000000-0000-4000-8000-0000000ca202';
const PREFIX = 'mf_dav_00000000-0000-4000-8000-00000000da02';
const HASH = '$2a$04$.DTOk/9ufpaOXB.cN0LyfuM.YcEc0lfE2EOEqfGQK9ORry8.bFOt.';
const PASSWORD = `${PREFIX}.Secret_secret_1234`;
const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const vcard = (uid, name, email, extra = '') => `BEGIN:VCARD\r\nVERSION:3.0\r\nUID:${uid}\r\nFN:${name}\r\nEMAIL:${email}\r\n${extra}END:VCARD\r\n`;
const ics = (uid, summary, start) => `BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:${uid}\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:${start}\r\nDURATION:PT1H\r\nSUMMARY:${summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n`;

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'`);
  await sql('DELETE FROM calendar_import_sources WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql('DELETE FROM dav_app_passwords WHERE user_id = $1', [U.alice]);
  await sql(`INSERT INTO dav_app_passwords (user_id, label, token_prefix, secret_hash) VALUES ($1, 'Sync', $2, $3)`, [U.alice, PREFIX, HASH]);
  await sql('DELETE FROM contacts WHERE user_id = $1', [U.alice]);
  await sql('DELETE FROM address_books WHERE user_id = $1', [U.alice]);
  await sql(`INSERT INTO address_books (id, user_id, name, source, created_at) VALUES ($1, $2, 'Friends', 'local', '2026-01-01')`, [BOOK, U.alice]);
  for (const [uid, name, email, extra] of [['r-1', 'Remote One', 'One@X.test', 'TITLE:Boss\r\n'], ['r-2', 'Remote Two', 'one@x.test', ''], ['r-3', 'Remote Three', 'three@x.test', 'BDAY:1990-01-02\r\n']]) {
    const card = vcard(uid, name, email, extra);
    await sql(`INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name) VALUES ($1, $2, $3, $4, md5($4), $5)`, [BOOK, U.alice, uid, card, name]);
  }
  await sql('DELETE FROM calendars WHERE user_id = $1', [U.alice]);
  await sql(`INSERT INTO calendars (id, user_id, owner_user_id, name, color, source, created_at) VALUES ($1, $3, $3, 'Team', '#336699', 'local', '2026-01-01'), ($2, $3, $3, 'Empty', NULL, 'local', '2026-01-02')`, [CAL, CAL2, U.alice]);
  for (const [uid, summary, start] of [['ev-1', 'Kickoff', '20261105T090000Z'], ['ev-2', 'Retro', '20261112T150000Z']]) {
    await sql(`INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at) VALUES ($1, $2, $3, $4, md5($4), $5, now(), now())`, [CAL, U.alice, uid, ics(uid, summary, start), summary]);
  }
  return { keep: [U.alice, U.bob, BOOK, CAL, CAL2] };
}

const post = (path, body, extra = {}) => ({ method: 'POST', path, as: 'alice', body, ...extra });
const creds = { url: `${REMOTE}/`, username: 'alice@example.test', password: PASSWORD };
const calendars = [
  { href: `${REMOTE}/caldav/${U.alice}/${CAL}/`, displayName: 'Team', color: '#336699' },
  { href: `${REMOTE}/caldav/${U.alice}/${CAL2}/`, displayName: '', selected: false },
];
const addressBooks = [{ href: `${REMOTE}/carddav/${U.alice}/${BOOK}/`, displayName: 'Friends' }];

export default [{
  name: 'DAV servers: discover, import, sync, update, remove',
  seed,
  sessions,
  steps: [
    { path: '/api/dav/servers' },
    { path: '/api/dav/servers', as: 'alice' },
    post('/api/dav/servers/discover', { username: 'alice@example.test' }),
    post('/api/dav/servers/discover', { username: 'alice', password: 'x' }),
    post('/api/dav/servers/discover', { url: 'http://a:b@127.0.0.1/', username: 'alice', password: 'x' }),
    post('/api/dav/servers/discover', { url: 'ftp://127.0.0.1/', username: 'alice', password: 'x' }),
    post('/api/dav/servers/discover', { url: 'not a url', username: 'alice', password: 'x' }),
    post('/api/dav/servers/discover', { ...creds, password: 'mf_dav_wrong' }),
    post('/api/dav/servers/discover', creds),
    post('/api/dav/servers', { ...creds, displayName: 'Home server' }),
    post('/api/dav/servers', { ...creds, calendars: [] }),
    post('/api/dav/servers', { ...creds, displayName: 'Bad href', calendars: [{ displayName: 'x' }] }),
    post('/api/dav/servers', { ...creds, displayName: 'Bad href', addressBooks: [{ href: 'nope' }] }),
    post('/api/dav/servers', { ...creds, displayName: 'Home server', intervalSeconds: 5, calendars, addressBooks }, { capture: (j) => ({ id: j.server?.id }), waitMs: 50 }),
    post('/api/dav/servers', { ...creds, displayName: 'Again', calendars }),
    { path: '/api/dav/servers', as: 'alice' },
    { path: '/api/dav/servers', as: 'bob' },
    (c) => post(`/api/dav/servers/${c.id}/sync`, {}),
    (c) => post(`/api/dav/servers/${c.id}/sync`, {}, { as: 'bob' }),
    (c) => ({ method: 'PATCH', path: `/api/dav/servers/${c.id}`, as: 'alice', body: {} }),
    (c) => ({ method: 'PATCH', path: `/api/dav/servers/${c.id}`, as: 'alice', body: { displayName: '  ' } }),
    (c) => ({ method: 'PATCH', path: `/api/dav/servers/${c.id}`, as: 'alice', body: { intervalSeconds: 9 } }),
    (c) => ({ method: 'PATCH', path: `/api/dav/servers/${c.id}`, as: 'alice', body: { calendars: [{ href: 'bad' }] } }),
    (c) => ({ method: 'PATCH', path: `/api/dav/servers/${c.id}`, as: 'alice', body: { displayName: ' Renamed ', intervalSeconds: '600', writable: true, addressBooks: [{ id: 'unknown', selected: true }] } }),
    { path: '/api/calendar/calendars', as: 'alice' },
    { path: '/api/contacts/address-books', as: 'alice' },
    (c) => ({ method: 'PATCH', path: `/api/dav/servers/${c.id}`, as: 'alice', body: { addressBooks: [{ href: addressBooks[0].href, displayName: 'Friends', selected: false }], enabled: false } }),
    { path: '/api/contacts/address-books', as: 'alice' },
    (c) => ({ method: 'DELETE', path: `/api/dav/servers/${c.id}`, as: 'bob' }),
    (c) => ({ method: 'DELETE', path: `/api/dav/servers/${c.id}`, as: 'alice' }),
    (c) => ({ method: 'DELETE', path: `/api/dav/servers/${c.id}`, as: 'alice' }),
    { path: '/api/calendar/calendars', as: 'alice' },
  ],
  check: {
    8: ({ result }) => { if (result.status !== 200 || !result.json?.calendars?.length || !result.json?.addressBooks?.length) throw new Error(`discovery must find collections: ${JSON.stringify(result.json)}`); },
    13: ({ result }) => { if (result.status !== 201 || result.json?.calendarSync?.eventCount !== 2 || result.json?.addressBookSync?.contactCount !== 3) throw new Error(`import must sync: ${JSON.stringify(result.json)}`); },
  },
  after: async ({ sql }) => ({
    sources: (await sql('SELECT kind, display_name, interval_seconds, writable, enabled, url LIKE \'enc:v1:%\' AS url_encrypted, password LIKE \'enc:v1:%\' AS password_encrypted FROM calendar_import_sources WHERE user_id = $1', [U.alice])).rows,
    calendars: (await sql(`SELECT name, color, source, read_only, external_url IS NOT NULL AS external FROM calendars WHERE user_id = $1 ORDER BY name`, [U.alice])).rows,
    books: (await sql(`SELECT name, source FROM address_books WHERE user_id = $1 ORDER BY name`, [U.alice])).rows,
  }),
}, {
  name: 'DAV servers: imported calendars and contacts',
  seed,
  sessions,
  steps: [
    post('/api/dav/servers', { ...creds, displayName: 'Import', calendars, addressBooks }),
    { path: '/api/contacts?limit=50', as: 'alice', ignore: ['contacts'] },
  ],
  after: async ({ sql }) => ({
    events: (await sql(`SELECT e.uid, e.summary, e.starts_at, e.ends_at, e.remote_href, e.remote_etag, c.name AS calendar, c.read_only FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id WHERE e.user_id = $1 AND c.source = 'caldav' ORDER BY e.uid`, [U.alice])).rows,
    contacts: (await sql(`SELECT c.uid, c.display_name, c.primary_email, c.emails, c.title, c.birthday::text, c.etag, b.name AS book FROM contacts c JOIN address_books b ON b.id = c.address_book_id WHERE c.user_id = $1 AND b.source = 'carddav' ORDER BY c.uid`, [U.alice])).rows,
    status: (await sql('SELECT last_error, address_book_last_error, last_sync_at IS NOT NULL AS synced FROM calendar_import_sources WHERE user_id = $1', [U.alice])).rows,
  }),
}];
