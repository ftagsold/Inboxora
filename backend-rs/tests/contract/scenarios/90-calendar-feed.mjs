// /calendar/feeds/:token(.ics) and /api/calendar/feeds
import { U } from '../seed-mail.mjs';

const CAL = '00000000-0000-4000-8000-0000000cf001';
const EMPTY_CAL = '00000000-0000-4000-8000-0000000cf002';
const BOB_CAL = '00000000-0000-4000-8000-0000000cf003';
const EV = (n) => `00000000-0000-4000-8000-0000000fe${String(n).padStart(3, '0')}`;
const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const ICS_HEADERS = ['content-type', 'cache-control', 'etag', 'retry-after', 'x-content-type-options'];

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql('DELETE FROM calendar_secret_feeds WHERE owner_user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql('DELETE FROM calendars WHERE user_id IN ($1, $2)', [U.alice, U.bob]);
  await sql(`INSERT INTO calendars (id, user_id, owner_user_id, name, source, created_at) VALUES ($1, $4, $4, 'Work, Team', 'local', '2026-01-01'), ($2, $4, $4, 'Ärger', 'local', '2026-01-02'), ($3, $5, $5, 'Bob', 'local', '2026-01-03')`, [CAL, EMPTY_CAL, BOB_CAL, U.alice, U.bob]);
  // Mon 2 Nov, Fri 6 Nov, Sat 31 Oct: Node orders the feed by Date#toString(), weekday first.
  const events = [
    [1, 'mon', '2026-11-02T09:00:00Z', '2026-11-02T10:00:00Z', false, 'Monday; sync', 'Plain\nnotes, with comma', null],
    [2, 'fri', '2026-11-06T09:00:00.250Z', '2026-11-06T10:00:00Z', false, 'Ünïcode '.repeat(12), '<p>Rich <b>text</b></p>', 'https://x.test/a?b=c,d'],
    [3, '', '2026-10-31T00:00:00Z', '2026-11-01T00:00:00Z', true, null, null, null],
  ];
  for (const [n, uid, s, e, allDay, summary, description, url] of events) {
    await sql(`INSERT INTO calendar_events (id, calendar_id, user_id, uid, raw_ical, summary, description, url, location, starts_at, ends_at, all_day, created_at, updated_at) VALUES ($1, $2, $3, $4, '', $5, $6, $7, $8, $9, $10, $11, '2026-01-01T00:00:00Z', $12)`,
      [EV(n), CAL, U.alice, uid || EV(n), summary, description, url, n === 1 ? 'Room 1' : null, s, e, allDay, n === 2 ? '2026-03-01T00:00:00Z' : '2026-02-03T04:05:06.789Z']);
  }
  await sql(`INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, summary, starts_at, ends_at) VALUES ($1, $2, 'bob-secret', '', 'Bob only', '2026-11-03T09:00:00Z', '2026-11-03T10:00:00Z')`, [BOB_CAL, U.bob]);
  return { keep: [U.alice, U.bob, CAL, EMPTY_CAL, BOB_CAL, EV(1), EV(2), EV(3)] };
}

const feed = (path, extra = {}) => ({ path, compareHeaders: ICS_HEADERS, ...extra });

export default [{
  name: 'secret feeds: issue, serve, revalidate, rotate, revoke',
  seed,
  sessions,
  ignoreKeys: ['secret', 'url'],
  steps: [
    { method: 'POST', path: '/api/calendar/feeds', body: { calendarIds: [CAL] } },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: {} },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: { calendarIds: [] } },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: { calendarIds: [CAL, 7] } },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: { calendarIds: [CAL, BOB_CAL] } },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: { calendarIds: ['nope'] } },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: { calendarIds: [CAL, EMPTY_CAL, CAL] }, capture: (j) => ({ token: j.secret, id: j.feed.id }) },
    { method: 'POST', path: '/api/calendar/feeds', as: 'alice', body: { calendarIds: [EMPTY_CAL] }, capture: (j) => ({ emptyToken: j.secret }) },
    { path: '/api/calendar/feeds', as: 'alice' },
    (c) => feed(`/calendar/feeds/${c.token}.ics`, { exactEtag: true }),
    (c) => feed(`/calendar/feeds/${c.token}`),
    (c) => feed(`/calendar/feeds/${c.token}.ICS/`),
    (c) => feed(`/calendar/feeds/${c.emptyToken}.ics`, { exactEtag: true }),
    (c) => feed(`/calendar/feeds/${c.token}.ics`, { headers: { 'if-none-match': '*' } }),
    { path: '/calendar/feeds/short.ics', compareHeaders: ICS_HEADERS },
    { path: `/calendar/feeds/${'A'.repeat(43)}.ics`, compareHeaders: ICS_HEADERS },
    { path: '/calendar/feeds/%E0.ics' },
    { path: '/calendar/feeds/a/b' },
    (c) => ({ method: 'POST', path: `/api/calendar/feeds/${c.id}/rotate`, as: 'bob' }),
    (c) => ({ method: 'POST', path: `/api/calendar/feeds/${c.id}/rotate`, as: 'alice', capture: (j) => ({ rotated: j.secret }) }),
    (c) => feed(`/calendar/feeds/${c.token}.ics`),
    (c) => feed(`/calendar/feeds/${c.rotated}.ics`),
    (c) => ({ method: 'DELETE', path: `/api/calendar/feeds/${c.id}`, as: 'bob' }),
    (c) => ({ method: 'DELETE', path: `/api/calendar/feeds/${c.id}`, as: 'alice' }),
    (c) => ({ method: 'DELETE', path: `/api/calendar/feeds/${c.id}`, as: 'alice' }),
    (c) => feed(`/calendar/feeds/${c.rotated}.ics`),
    { method: 'DELETE', path: '/api/calendar/feeds/not-a-uuid', as: 'alice' },
    { path: '/api/calendar/feeds/whatever', as: 'alice' },
  ],
  check: {
    9: ({ result }) => {
      const t = result.text || '';
      const order = ['UID:fri', 'UID:mon', `UID:${EV(3)}`].map(u => t.indexOf(u));
      if (result.status !== 200 || order.some(i => i < 0) || !(order[0] < order[1] && order[1] < order[2])) throw new Error(`feed must follow Node's toString order:\n${t}`);
    },
    13: ({ result }) => { if (result.status !== 304) throw new Error(`If-None-Match: * must answer 304, got ${result.status}`); },
  },
  after: async ({ sql }) => (await sql('SELECT calendar_ids, revoked_at IS NOT NULL AS revoked FROM calendar_secret_feeds WHERE owner_user_id = $1 ORDER BY cardinality(calendar_ids) DESC', [U.alice])).rows,
}];
