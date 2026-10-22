// /api/calendar/sources: iCalendar URL and CalDAV imports. The remote for both backends is the
// Node backend itself (its secret feed and its /caldav), so remote URLs are identical.
import { createHash } from 'node:crypto';
import { U } from '../seed-mail.mjs';
import { BACKENDS } from '../harness.mjs';

const REMOTE = `http://127.0.0.1:${BACKENDS.node.port}`;
const CAL = '00000000-0000-4000-8000-0000000ca301';
const TOKEN = 'ContractFeedToken_'.padEnd(43, 'x');
const FEED_URL = `${REMOTE}/calendar/feeds/${TOKEN}.ics`;
const PREFIX = 'mf_dav_00000000-0000-4000-8000-00000000da03';
const HASH = '$2a$04$.DTOk/9ufpaOXB.cN0LyfuM.YcEc0lfE2EOEqfGQK9ORry8.bFOt.';
const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const ics = (uid, summary, start, end) => `BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:${uid}\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:${start}\r\nDTEND:${end}\r\nSUMMARY:${summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n`;

async function seed({ sql }) {
  if (TOKEN.length !== 43) throw new Error('feed token must be 43 characters');
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'`);
  await sql('DELETE FROM calendar_import_sources WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql('DELETE FROM calendar_secret_feeds WHERE owner_user_id = $1', [U.alice]);
  await sql('DELETE FROM dav_app_passwords WHERE user_id = $1', [U.alice]);
  await sql(`INSERT INTO dav_app_passwords (user_id, label, token_prefix, secret_hash) VALUES ($1, 'Sync', $2, $3)`, [U.alice, PREFIX, HASH]);
  await sql('DELETE FROM calendars WHERE user_id = $1', [U.alice]);
  await sql(`INSERT INTO calendars (id, user_id, owner_user_id, name, source, created_at) VALUES ($1, $2, $2, 'Origin', 'local', '2026-01-01')`, [CAL, U.alice]);
  for (const [uid, summary, s, e] of [['f-1', 'Planning', '2026-11-02T09:00:00Z', '2026-11-02T10:00:00Z'], ['f-2', 'Review', '2026-11-09T09:00:00Z', '2026-11-09T10:30:00Z']]) {
    const raw = ics(uid, summary, s.replace(/[-:]/g, '').replace('.000', ''), e.replace(/[-:]/g, ''));
    await sql(`INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, created_at, updated_at) VALUES ($1, $2, $3, $4, md5($4), $5, $6, $7, '2026-01-01', '2026-01-02')`, [CAL, U.alice, uid, raw, summary, s, e]);
  }
  await sql('INSERT INTO calendar_secret_feeds (owner_user_id, token_hash, calendar_ids) VALUES ($1, $2, $3)', [U.alice, createHash('sha256').update(TOKEN).digest('hex'), [CAL]]);
  return { keep: [U.alice, U.bob, CAL] };
}

const post = (path, body, extra = {}) => ({ method: 'POST', path, as: 'alice', body, ...extra });

export default [{
  name: 'calendar sources: iCalendar URL and CalDAV',
  seed,
  sessions,
  steps: [
    { path: '/api/calendar/sources' },
    post('/api/calendar/sources', { kind: 'ics', displayName: 'x' }),
    post('/api/calendar/sources', { kind: 'ical_url', displayName: 'Feed' }),
    post('/api/calendar/sources', { kind: 'caldav', displayName: 'Dav', username: 'alice@example.test' }),
    post('/api/calendar/sources', { kind: 'ical_url', displayName: 'Feed', url: 'webcal://user:pw@127.0.0.1/x.ics' }),
    post('/api/calendar/sources', { kind: 'ical_url', displayName: 'Bad feed', url: `${REMOTE}/calendar/feeds/${'z'.repeat(43)}.ics` }),
    post('/api/calendar/sources', { kind: 'ical_url', displayName: 'Feed', url: FEED_URL, color: '#123456', intervalSeconds: 3600 }, { capture: (j) => ({ feed: j.source?.id }) }),
    post('/api/calendar/sources', { kind: 'ical_url', displayName: 'Feed again', url: FEED_URL }),
    post('/api/calendar/sources', { kind: 'caldav', displayName: 'Origin DAV', url: `${REMOTE}/caldav/`, username: 'alice@example.test', password: `${PREFIX}.Secret_secret_1234`, writable: true,
      collections: [{ href: `${REMOTE}/caldav/${U.alice}/${CAL}/`, displayName: 'Origin', color: '#abcdef' }] }, { capture: (j) => ({ dav: j.source?.id }) }),
    { path: '/api/calendar/sources', as: 'alice' },
    { path: '/api/calendar/sources', as: 'bob' },
    { path: '/api/calendar/calendars', as: 'alice' },
    (c) => post(`/api/calendar/sources/${c.feed}/sync`, {}),
    (c) => post(`/api/calendar/sources/${c.feed}/sync`, {}, { as: 'bob' }),
    (c) => ({ method: 'PATCH', path: `/api/calendar/sources/${c.feed}`, as: 'alice', body: { writable: true } }),
    (c) => ({ method: 'PATCH', path: `/api/calendar/sources/${c.feed}`, as: 'alice', body: { color: '', username: '', displayName: ' Renamed feed ' } }),
    (c) => ({ method: 'PATCH', path: `/api/calendar/sources/${c.dav}`, as: 'alice', body: { writable: false, collections: [{ id: 'nope', selected: false }] } }),
    (c) => ({ method: 'PATCH', path: `/api/calendar/sources/${c.dav}`, as: 'alice', body: { enabled: false } }),
    (c) => ({ method: 'DELETE', path: `/api/calendar/sources/${c.feed}`, as: 'bob' }),
    (c) => ({ method: 'DELETE', path: `/api/calendar/sources/${c.feed}`, as: 'alice' }),
    { path: '/api/calendar/sources', as: 'alice' },
  ],
  check: {
    6: ({ result }) => { if (result.status !== 201 || result.json?.sync?.eventCount !== 2) throw new Error(`feed import must sync two events: ${JSON.stringify(result.json)}`); },
    8: ({ result }) => { if (result.status !== 201 || result.json?.sync?.eventCount !== 2) throw new Error(`CalDAV import must sync two events: ${JSON.stringify(result.json)}`); },
  },
  after: async ({ sql }) => ({
    events: (await sql(`SELECT e.uid, e.summary, e.starts_at, e.ends_at, c.name, c.source, c.read_only, e.remote_href IS NOT NULL AS has_href, e.remote_etag IS NOT NULL AS has_etag FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id WHERE e.user_id = $1 ORDER BY c.name, e.uid`, [U.alice])).rows,
    documents: (await sql('SELECT length(raw_ical) > 0 AS stored FROM calendar_import_documents d JOIN calendar_import_sources s ON s.id = d.source_id WHERE s.user_id = $1', [U.alice])).rows,
    sources: (await sql('SELECT kind, display_name, color, username, writable, enabled, last_error FROM calendar_import_sources WHERE user_id = $1 ORDER BY kind', [U.alice])).rows,
  }),
}];
