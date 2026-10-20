// Event-listing behaviour for the calendar endpoint: calendar selection stays
// scoped to the owner, an explicit empty selection means "none", and an
// incomplete projection is reported rather than silently shortened.

import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { listeningPort } from '../test/net.js';
import type { Server } from 'node:http';
import type { NextFunction, Request, Response } from 'express';
import type { query as queryContract } from '../services/db.js';
import 'express-async-errors';

type QueryCall = Parameters<typeof queryContract>;

const { query } = vi.hoisted(() => ({ query: vi.fn<typeof queryContract>() }));
vi.mock('../services/db.js', () => ({ query, withTransaction: vi.fn(async (fn) => fn({ query })) }));
vi.mock('../services/encryption.js', () => ({ encrypt: (value: string) => `enc:${value}`, decrypt: (value: string) => value, }));
vi.mock('../services/calendarInvitation.js', () => ({ sendCalendarInvitation: vi.fn() }));
vi.mock('../services/externalCalendarSync.js', () => ({ releaseCalendarSource: vi.fn(), scheduleCalendarSource: vi.fn(), stopCalendarSource: vi.fn(), syncCalendarSource: vi.fn() }));
vi.mock('../middleware/auth.js', () => ({ requireAuth: (req: { headers: Record<string, string>; session?: { userId?: string } }, _res: unknown, next: () => void) => { req.session = { userId: 'user-1' }; next(); } }));
vi.mock('../services/hostValidation.js', () => ({ validateHost: vi.fn(async () => null) }));
vi.mock('../services/connectionPolicy.js', () => ({ getConnectionPolicy: vi.fn(async () => ({ allowPrivateHosts: false })) }));

import express from 'express';
import calendarRouter from './calendar.js';

let server: Server;
let base = '';

beforeAll(async () => {
  const app = express();
  app.use(express.json());
  app.use('/api/calendar', calendarRouter);
  app.use((error: Error, _req: Request, res: Response, next: NextFunction) => { void next; return res.status(500).json({ error: error.message }); });
  await new Promise((resolve) => { server = app.listen(0, resolve); });
  base = `http://127.0.0.1:${listeningPort(server)}`;
});

afterAll(async () => {
  await new Promise((resolve) => server.close(resolve));
});

beforeEach(() => {
  query.mockReset();
  query.mockResolvedValue({ rows: [] });
});

const RANGE = 'from=2026-09-01T00:00:00.000Z&to=2026-10-01T00:00:00.000Z';
const calendarId = '11111111-1111-4111-8111-111111111111';

type CalendarEventsResponse = {
  events: unknown[];
  truncated: boolean;
  incompleteSeries?: unknown[];
};

function isCalendarEventsResponse(value: unknown): value is CalendarEventsResponse {
  if (typeof value !== 'object' || value === null || !('events' in value) || !('truncated' in value)) return false;
  const { events, truncated } = value;
  const incompleteSeries = 'incompleteSeries' in value ? value.incompleteSeries : undefined;
  return Array.isArray(events)
    && typeof truncated === 'boolean'
    && (incompleteSeries === undefined || Array.isArray(incompleteSeries));
}

async function calendarEventsResponse(response: globalThis.Response): Promise<CalendarEventsResponse> {
  const payload: unknown = await response.json();
  if (!isCalendarEventsResponse(payload)) throw new Error('Expected calendar events response');
  return payload;
}

function eventQuery(): QueryCall {
  const call = query.mock.calls.find(([sql]) => sql.includes('FROM calendar_events'));
  if (!call) throw new Error('Expected calendar event query');
  return call;
}

function queryParameters(call: QueryCall): unknown[] {
  const [, parameters] = call;
  if (!parameters) throw new Error('Expected query parameters');
  return parameters;
}

describe('GET /api/calendar/events calendar selection', () => {
  it('keeps the owner scope on every query', async () => {
    await fetch(`${base}/api/calendar/events?${RANGE}`);
    const call = eventQuery();
    expect(call[0]).toContain('e.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1');
    expect(queryParameters(call)[0]).toBe('user-1');
  });

  it('adds a parameterised calendar filter only when a selection is supplied', async () => {
    await fetch(`${base}/api/calendar/events?${RANGE}`);
    const unfiltered = eventQuery();
    expect(unfiltered[0]).not.toContain('ANY($4::uuid[])');

    query.mockClear();
    await fetch(`${base}/api/calendar/events?${RANGE}&calendarIds=${calendarId}`);
    const filtered = eventQuery();
    expect(filtered[0]).toContain('c.id = ANY($4::uuid[])');
    expect(queryParameters(filtered)[3]).toEqual([calendarId]);
  });

  it('treats an explicitly empty selection as no calendars at all', async () => {
    const response = await fetch(`${base}/api/calendar/events?${RANGE}&calendarIds=`);
    expect(response.status).toBe(200);
    expect(await calendarEventsResponse(response)).toEqual({ events: [], truncated: false });
    // No event query at all: an empty selection cannot match a calendar.
    expect(query.mock.calls.some(([sql]) => sql.includes('FROM calendar_events'))).toBe(false);
  });

  it('skips the event query when only the contact calendar is selected', async () => {
    const response = await fetch(`${base}/api/calendar/events?${RANGE}&calendarIds=contacts-birthdays`);
    expect(response.status).toBe(200);
    expect(query.mock.calls.some(([sql]) => sql.includes('FROM calendar_events'))).toBe(false);
    expect(query.mock.calls.some(([sql]) => sql.includes('FROM contacts'))).toBe(true);
  });

  it('rejects a malformed calendar id before touching the database', async () => {
    const response = await fetch(`${base}/api/calendar/events?${RANGE}&calendarIds=not-a-uuid`);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: 'Invalid calendar id' });
    expect(query).not.toHaveBeenCalled();
  });

  it('accepts a comma-separated selection and de-duplicates it', async () => {
    const other = '22222222-2222-4222-8222-222222222222';
    await fetch(`${base}/api/calendar/events?${RANGE}&calendarIds=${calendarId},${other},${calendarId}`);
    const filtered = eventQuery();
    expect(queryParameters(filtered)[3]).toEqual([calendarId, other]);
  });

  it('finds recurring series through the indexed column, never a regex over raw_ical', async () => {
    await fetch(`${base}/api/calendar/events?${RANGE}`);
    const call = eventQuery();
    // The recurring half of "in this window, or a series" decides whether the planner can
    // use an index at all. As a regex over an unindexed TEXT column it could not, so every
    // event the user owned was scanned and its raw_ical detoasted — measured at 131 ms
    // against 3 ms on 20k events. `recurring` is maintained by a trigger (migration 0082).
    expect(call[0]).toContain('OR e.recurring');
    expect(call[0]).not.toMatch(/raw_ical\s*~\*/);
  });

  it('resolves the source message of a mail invitation only for its own account', async () => {
    await fetch(`${base}/api/calendar/events?${RANGE}`);
    // The read path has two queries now — materialised occurrences and the live fallback — and
    // both expose the mail link, so both must carry the tenant-safe join. Checking only the
    // first match would let a regression through in whichever one moved.
    const eventQueries = query.mock.calls.filter(([sql]) => sql.includes('FROM calendar_events'));
    expect(eventQueries.length).toBeGreaterThan(0);
    for (const [sql] of eventQueries) {
      // The link back to the original mail must not be able to cross tenants, and it
      // must disappear when the message is gone instead of pointing at a dead id.
      expect(sql).toContain('LEFT JOIN messages sm ON sm.id = e.source_message_id');
      expect(sql).toContain('LEFT JOIN email_accounts sa ON sa.id = sm.account_id AND sa.user_id = e.user_id');
      expect(sql).toContain('CASE WHEN sa.id IS NOT NULL THEN e.source_message_id END AS source_message_id');
    }
  });

  // The read path has two queries, and an event reaches the client through whichever one covers
  // it. They must expose the same contract: dropping a field from one makes that event render
  // differently for no visible reason — which is exactly how events from one calendar started
  // appearing in the default colour while their neighbours kept the calendar's own.
  it('exposes the same calendar metadata from both read paths', async () => {
    await fetch(`${base}/api/calendar/events?${RANGE}`);
    const eventQueries = query.mock.calls.filter(([sql]) => sql.includes('FROM calendar_events') || sql.includes('FROM calendar_occurrences o'));
    expect(eventQueries.length).toBe(2);
    for (const [sql] of eventQueries) {
      // raw_ical is deliberately absent from the materialised path: stored occurrences do not
      // need it, and it must never reach the client.
      for (const field of ['calendar_name', 'calendar_color', 'source', 'read_only', 'source_message_id', 'source_folder', 'source_account_id']) {
        expect(sql).toContain(field);
      }
      // Colour and editability come from the calendar row, so both paths must join it.
      expect(sql).toContain('JOIN calendars c ON c.id =');
    }
  });

  it('exposes the source message folder and account so the reader can be opened', async () => {
    query.mockImplementation(async (sql: string) => {
      if (sql.includes('FROM calendar_events')) {
        return { rows: [{
          id: 'row-mail', calendar_id: calendarId, uid: 'mail-1', etag: 'etag-1',
          raw_ical: ['BEGIN:VCALENDAR', 'VERSION:2.0', 'BEGIN:VEVENT', 'UID:mail-1', 'DTSTAMP:20260901T000000Z',
            'DTSTART:20260910T090000Z', 'DTEND:20260910T100000Z', 'SUMMARY:Z zaproszenia', 'END:VEVENT', 'END:VCALENDAR', ''].join('\r\n'),
          summary: 'Z zaproszenia', starts_at: new Date('2026-09-10T09:00:00Z'), ends_at: new Date('2026-09-10T10:00:00Z'),
          all_day: false, attendees: [], source_message_id: 'copy-1', source_folder: 'INBOX', source_account_id: 'account-1',
          calendar_name: 'Prywatny', calendar_color: '#4b75ff', source: 'local', read_only: false,
        }] };
      }
      return { rows: [] };
    });

    const response = await fetch(`${base}/api/calendar/events?${RANGE}`);
    const { events } = await calendarEventsResponse(response);
    if (events.length === 0) throw new Error('Expected calendar events in response');
    expect(events[0]).toMatchObject({
      summary: 'Z zaproszenia', source_message_id: 'copy-1', source_folder: 'INBOX', source_account_id: 'account-1',
    });
  });
});

describe('GET /api/calendar/events projection outcome', () => {
  it('reports a complete result with an explicit truncated:false', async () => {
    const response = await fetch(`${base}/api/calendar/events?${RANGE}`);
    expect(await calendarEventsResponse(response)).toEqual({ events: [], truncated: false });
  });

  it('reports an incomplete series without leaking internal error text', async () => {
    query.mockImplementation(async (sql: string) => {
      if (sql.includes('FROM calendar_events')) {
        return { rows: [{
          id: 'row-dense', calendar_id: calendarId, uid: 'dense', etag: 'etag-1',
          raw_ical: ['BEGIN:VCALENDAR', 'VERSION:2.0', 'BEGIN:VEVENT', 'UID:dense', 'DTSTAMP:20200101T000000Z',
            'DTSTART:19700101T000000Z', 'DTEND:19700101T000100Z', 'RRULE:FREQ=MINUTELY', 'SUMMARY:Dense', 'END:VEVENT', 'END:VCALENDAR', ''].join('\r\n'),
          summary: 'Dense', starts_at: new Date('1970-01-01T00:00:00Z'), ends_at: new Date('1970-01-01T00:01:00Z'), all_day: false,
        }] };
      }
      return { rows: [] };
    });
    const previous = process.env.CALENDAR_PROJECTION_MAX_ITERATIONS;
    process.env.CALENDAR_PROJECTION_MAX_ITERATIONS = '500';
    try {
      const response = await fetch(`${base}/api/calendar/events?${RANGE}`);
      const payload = await calendarEventsResponse(response);
      expect(response.status).toBe(200);
      expect(payload.truncated).toBe(true);
      expect(payload.incompleteSeries).toEqual([{ series_id: 'row-dense', reason: 'iteration-limit' }]);
      // The marker is present and stable; no stack trace or SQL detail is exposed.
      expect(JSON.stringify(payload)).not.toMatch(/stack|at Object|SELECT|FROM calendar_events/);
    } finally {
      if (previous === undefined) delete process.env.CALENDAR_PROJECTION_MAX_ITERATIONS;
      else process.env.CALENDAR_PROJECTION_MAX_ITERATIONS = previous;
    }
  });
});

describe('GET /api/calendar/events/search', () => {
  // The search match itself is a plain calendar_events read — no occurrence-table join and no
  // date/horizon filter of any kind, which is the whole point: a match must never depend on
  // whether (or how far) the event has been materialised. See searchResponse() below for how
  // ordering/limit are checked instead — those now happen after projection, in JS, not in SQL.
  function searchQuery(): QueryCall {
    const call = query.mock.calls.find(([sql]) => sql.includes('FROM calendar_events e') && sql.includes('ILIKE'));
    if (!call) throw new Error('Expected a calendar_events search query');
    return call;
  }

  async function searchResponse(q: string, extra = ''): Promise<{ events: Array<Record<string, unknown>> }> {
    const response = await fetch(`${base}/api/calendar/events/search?q=${encodeURIComponent(q)}${extra}`);
    expect(response.status).toBe(200);
    return response.json() as Promise<{ events: Array<Record<string, unknown>> }>;
  }

  it('returns no results and issues no query for an empty or missing q', async () => {
    const empty = await fetch(`${base}/api/calendar/events/search`);
    expect(empty.status).toBe(200);
    expect(await empty.json()).toEqual({ events: [] });
    expect(query).not.toHaveBeenCalled();

    const blank = await fetch(`${base}/api/calendar/events/search?q=%20%20`);
    expect(blank.status).toBe(200);
    expect(await blank.json()).toEqual({ events: [] });
    expect(query).not.toHaveBeenCalled();
  });

  it('rejects a query over 200 characters before touching the database', async () => {
    const response = await fetch(`${base}/api/calendar/events/search?q=${'a'.repeat(201)}`);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: 'Search query is too long' });
    expect(query).not.toHaveBeenCalled();
  });

  it('scopes the match to calendar_events owned by this user, with no occurrence-window restriction', async () => {
    await fetch(`${base}/api/calendar/events/search?q=standup`);
    const call = searchQuery();
    expect(call[0]).toContain('e.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1');
    const [sql, parameters] = call;
    expect(sql).toContain('JOIN calendars c ON c.id = e.calendar_id');
    // No window/horizon predicate of any kind on the match itself — only the owner-scope and
    // the ILIKE term constrain which rows match (starts_at/ends_at appear in the SELECT list,
    // as plain data the projector needs, but never in the WHERE clause).
    const where = sql.slice(sql.indexOf('WHERE'));
    expect(where).not.toMatch(/starts_at|ends_at|built_from|built_to/);
    expect(parameters?.[0]).toBe('user-1');
  });

  it('wraps the term for a case-insensitive substring match across summary, description and location', async () => {
    await fetch(`${base}/api/calendar/events/search?q=${encodeURIComponent('Team Sync')}`);
    const [sql, parameters] = searchQuery();
    expect(sql).toContain('e.summary ILIKE $2');
    expect(sql).toContain('e.description ILIKE $2');
    expect(sql).toContain('e.location ILIKE $2');
    expect(parameters?.[1]).toBe('%Team Sync%');
  });

  it('binds only the owner id and the search term — no window/limit of any kind reaches the SQL', async () => {
    await fetch(`${base}/api/calendar/events/search?q=standup&limit=10000`);
    const [sql] = searchQuery();
    expect(sql).not.toMatch(/\$3|\$4/); // only $1 (owner id) and $2 (the ILIKE term) are bound
  });

  it('orders results by start date descending — latest first — regardless of how far outside any horizon they fall', async () => {
    query.mockResolvedValue({ rows: [
      { id: 'past', calendar_id: calendarId, summary: 'Standup', starts_at: new Date('2015-01-01T09:00:00Z'), ends_at: new Date('2015-01-01T09:30:00Z'), all_day: false, calendar_name: 'Work', calendar_color: '#4b75ff', source: 'local', read_only: false },
      { id: 'future', calendar_id: calendarId, summary: 'Standup', starts_at: new Date('2099-01-01T09:00:00Z'), ends_at: new Date('2099-01-01T09:30:00Z'), all_day: false, calendar_name: 'Work', calendar_color: '#4b75ff', source: 'local', read_only: false },
      { id: 'near', calendar_id: calendarId, summary: 'Standup', starts_at: new Date('2026-09-10T09:00:00Z'), ends_at: new Date('2026-09-10T09:30:00Z'), all_day: false, calendar_name: 'Work', calendar_color: '#4b75ff', source: 'local', read_only: false },
    ] });
    const { events } = await searchResponse('standup');
    expect(events.map(event => event.id)).toEqual(['future', 'near', 'past']);
  });

  it('clamps an out-of-range limit after projection rather than passing it to SQL', async () => {
    query.mockResolvedValue({ rows: [
      { id: 'a', calendar_id: calendarId, summary: 'Standup', starts_at: new Date('2026-01-01T09:00:00Z'), ends_at: new Date('2026-01-01T09:30:00Z'), all_day: false },
      { id: 'b', calendar_id: calendarId, summary: 'Standup', starts_at: new Date('2026-02-01T09:00:00Z'), ends_at: new Date('2026-02-01T09:30:00Z'), all_day: false },
      { id: 'c', calendar_id: calendarId, summary: 'Standup', starts_at: new Date('2026-03-01T09:00:00Z'), ends_at: new Date('2026-03-01T09:30:00Z'), all_day: false },
    ] });
    expect((await searchResponse('standup', '&limit=0')).events).toHaveLength(1); // clamped up to the minimum of 1
    expect((await searchResponse('standup')).events).toHaveLength(3); // well under the 200 default
  });

  it('returns a matched non-recurring event using its own stored start/end, unchanged', async () => {
    // A unique id+etag (never reused by another test in this file) — projectCalendarResources
    // caches by (userId, row.id, row.etag, …), and a collision with another test's cache entry
    // would silently serve that test's stale result instead of actually re-projecting this one.
    query.mockResolvedValue({ rows: [{
      id: 'standup-single-1', calendar_id: calendarId, uid: 'standup-1', etag: 'etag-single-1', raw_ical: null,
      starts_at: new Date('2026-09-10T09:00:00Z'), ends_at: new Date('2026-09-10T09:30:00Z'), all_day: false,
      summary: 'Standup', calendar_name: 'Work', calendar_color: '#4b75ff', source: 'local', read_only: false,
    }] });
    const { events } = await searchResponse('standup');
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ id: 'standup-single-1', summary: 'Standup', calendar_name: 'Work' });
  });

  // This is the behaviour the horizon-based (materialised-occurrence) version of this route
  // could not offer: a recurring series matched by text expands to every one of its own
  // occurrences, including ones far outside the 3-month/18-month render-path window, because
  // the window handed to projectCalendarResources here is a century wide, not that one.
  it('expands a matched recurring series to occurrences both long past and far future', async () => {
    // Unique id+etag — see the note on the non-recurring test above for why reusing one from
    // another test in this file would silently serve that test's cached projection instead.
    query.mockResolvedValue({ rows: [{
      id: 'standup-series-1', calendar_id: calendarId, uid: 'standup-series', etag: 'etag-series-1',
      raw_ical: ['BEGIN:VCALENDAR', 'VERSION:2.0', 'BEGIN:VEVENT', 'UID:standup-series', 'DTSTAMP:20150101T000000Z',
        'DTSTART:20150105T090000Z', 'DTEND:20150105T093000Z', 'RRULE:FREQ=YEARLY;COUNT=20', 'SUMMARY:Annual standup', 'END:VEVENT', 'END:VCALENDAR', ''].join('\r\n'),
      summary: 'Annual standup', starts_at: new Date('2015-01-05T09:00:00Z'), ends_at: new Date('2015-01-05T09:30:00Z'), all_day: false,
      calendar_name: 'Work', calendar_color: '#4b75ff', source: 'local', read_only: false,
    }] });
    const { events } = await searchResponse('standup');
    // 20 yearly occurrences starting 2015-01-05 run through 2034 — well past the live render
    // path's 18-month-forward horizon and well before its 3-month-back one.
    expect(events).toHaveLength(20);
    expect(events.every(event => event.series_id === 'standup-series-1')).toBe(true);
    const years = events.map(event => new Date(String(event.starts_at)).getUTCFullYear());
    expect(Math.min(...years)).toBe(2015);
    expect(Math.max(...years)).toBe(2034);
    expect(years).toEqual([...years].sort((a, b) => b - a)); // descending, most recent on top
  });
});
