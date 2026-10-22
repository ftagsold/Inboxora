// /api/calendar (calendars and event reads)
import { readFileSync } from 'node:fs';
import { U } from '../seed-mail.mjs';

const docs = JSON.parse(readFileSync(new URL('../../differential/ical.docs.json', import.meta.url)));
const CAL = '00000000-0000-4000-8000-0000000ca001';
const CAL2 = '00000000-0000-4000-8000-0000000ca002';
const BOB_CAL = '00000000-0000-4000-8000-0000000ca003';
const EV = (n) => `00000000-0000-4000-8000-0000000e${String(n).padStart(4, '0')}`;
const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`UPDATE users SET preferences = '{}'::jsonb WHERE id = $1`, [U.alice]);
  await sql(`DELETE FROM calendars WHERE user_id IN ($1, $2)`, [U.alice, U.bob]);
  await sql(`DELETE FROM contacts WHERE user_id = $1`, [U.alice]);
  await sql(`INSERT INTO calendars (id, user_id, owner_user_id, name, color, source, created_at) VALUES ($1, $3, $3, 'Work', '#112233', 'local', '2026-01-01'), ($2, $3, $3, 'Feed', NULL, 'ical_url', '2026-01-02'), ($4, $5, $5, 'Bob', NULL, 'local', '2026-01-03')`, [CAL, CAL2, U.alice, BOB_CAL, U.bob]);
  const events = [
    [1, CAL, 0, 'weekly-berlin', '2026-03-02T08:00:00Z', '2026-03-02T09:00:00Z', false, 'Standup, team'],
    [2, CAL, 2, 'allday', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', true, 'New year'],
    [3, CAL2, 4, 'utc-single', '2026-10-22T07:00:00Z', '2026-10-22T08:00:00Z', false, 'Single'],
    [4, CAL, 3, 'outlook', '2026-10-20T13:00:00Z', '2026-10-20T14:00:00Z', false, 'Outlook series'],
    [5, CAL, 8, 'bad-rule', '2026-10-01T07:00:00Z', '2026-10-01T07:30:00Z', false, 'Bad rule'],
    [6, BOB_CAL, 4, 'bob-only', '2026-10-22T07:00:00Z', '2026-10-22T08:00:00Z', false, 'Bob secret'],
  ];
  for (const [n, cal, doc, uid, s, e, allDay, summary] of events) {
    await sql(`INSERT INTO calendar_events (id, calendar_id, user_id, uid, raw_ical, summary, starts_at, ends_at, all_day, etag) VALUES ($1, $2, (SELECT user_id FROM calendars WHERE id = $2), $3, $4, $5, $6, $7, $8, $9)`, [EV(n), cal, uid, docs[doc], summary, s, e, allDay, `etag-${n}`]);
  }
  await sql(`INSERT INTO address_books (user_id, name) VALUES ($1, 'Personal') ON CONFLICT DO NOTHING`, [U.alice]);
  await sql(`INSERT INTO contacts (address_book_id, user_id, uid, display_name, birthday, contact_dates) SELECT id, $1, 'bd', 'Birthday Person', '1980-03-10', '[{"label":"Wedding","value":"--03-12"},{"label":"Leap","value":"2000-02-29"}]'::jsonb FROM address_books WHERE user_id = $1 AND name = 'Personal'`, [U.alice]);
  return { keep: [CAL, CAL2, BOB_CAL, U.alice, U.bob, ...events.map(([n]) => EV(n))] };
}

const range = (from, to, extra = '') => `/api/calendar/events?from=${from}&to=${to}${extra}`;

export default [{
  name: 'calendars, event windows, projection, contacts, search',
  seed, sessions,
  steps: [
    { path: '/api/calendar/calendars' },
    { path: '/api/calendar/calendars', as: 'alice' },
    { path: range('2026-03-01T00:00:00.000Z', '2026-04-01T00:00:00.000Z'), as: 'alice' },
    { path: range('2026-01-01', '2027-01-01'), as: 'alice' },
    { path: range('2026-10-01T00:00:00Z', '2026-11-01T00:00:00Z', `&calendarIds=${CAL2}`), as: 'alice' },
    { path: range('2026-10-01T00:00:00Z', '2026-11-01T00:00:00Z', '&calendarIds=contacts-birthdays'), as: 'alice' },
    { path: range('2026-10-01T00:00:00Z', '2026-11-01T00:00:00Z', '&calendarIds='), as: 'alice' },
    { path: range('2026-10-01T00:00:00Z', '2026-11-01T00:00:00Z', '&calendarIds=nope'), as: 'alice' },
    { path: range('2026-10-01T00:00:00Z', '2026-11-01T00:00:00Z', `&calendarIds=${BOB_CAL}`), as: 'alice' },
    { path: range('2026-01-01', '2028-01-01'), as: 'alice' },
    { path: range('nope', '2026-01-01'), as: 'alice' },
    { path: range('2026-02-01', '2026-01-01'), as: 'alice' },
    { path: '/api/calendar/events/search?q=standup&limit=3', as: 'alice' },
    { path: '/api/calendar/events/search?q=secret', as: 'alice' },
    { path: '/api/calendar/events/search?q=', as: 'alice' },
    { path: `/api/calendar/events/${EV(1)}@2026-03-05T09:00:00/attendees`, as: 'alice' },
    { path: `/api/calendar/events/${EV(1)}/attendees`, as: 'bob' },
    { method: 'POST', path: '/api/calendar/calendars', as: 'alice', body: { name: '  Home ', color: '#abcdef' }, capture: (j) => ({ home: j.calendar.id }) },
    { method: 'POST', path: '/api/calendar/calendars', as: 'alice', body: { name: 'Home' } },
    { method: 'POST', path: '/api/calendar/calendars', as: 'alice', body: { name: 'X', color: 'red' } },
    { method: 'POST', path: '/api/calendar/calendars', as: 'alice', body: { name: 'X', displayVisible: 'yes' } },
    (c) => ({ method: 'PATCH', path: `/api/calendar/calendars/${c.home}`, as: 'alice', body: { name: 'House', color: null, displayVisible: false } }),
    (c) => ({ method: 'PATCH', path: `/api/calendar/calendars/${c.home}`, as: 'alice', body: { name: 'House' } }),
    { method: 'PATCH', path: '/api/calendar/calendars/contacts-birthdays', as: 'alice', body: { name: 'Birthdays', color: '#ff00ff', displayVisible: true } },
    { path: '/api/calendar/calendars', as: 'alice' },
    { path: range('2026-03-01T00:00:00Z', '2026-04-01T00:00:00Z', '&calendarIds=contacts-birthdays'), as: 'alice' },
    (c) => ({ method: 'DELETE', path: `/api/calendar/calendars/${c.home}`, as: 'alice', body: { confirmName: 'Wrong' } }),
    (c) => ({ method: 'DELETE', path: `/api/calendar/calendars/${c.home}`, as: 'alice', body: {} }),
    (c) => ({ method: 'DELETE', path: `/api/calendar/calendars/${c.home}`, as: 'alice', body: { confirmName: 'House' } }),
    { path: range('2026-01-01', '2027-01-01'), as: 'alice', waitMs: 7000 },
    { path: range('2026-03-01T00:00:00.000Z', '2026-04-01T00:00:00.000Z'), as: 'alice' },
  ],
  after: async ({ sql }) => (await sql(`SELECT e.uid, o.recurrence_id, o.starts_at, o.ends_at, o.summary FROM calendar_occurrences o JOIN calendar_events e ON e.id = o.event_id ORDER BY e.uid, o.starts_at`)).rows,
}];
