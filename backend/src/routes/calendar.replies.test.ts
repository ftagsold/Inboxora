// Answers (METHOD:REPLY) to invitations this user sent, and the attendee status read-out.
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { listeningPort } from '../test/net.js';
import type { Server } from 'node:http';
import type { NextFunction, Request, Response } from 'express';
import type { DbClient } from '../services/db.js';
import 'express-async-errors';

type QueryMock = DbClient['query'];
const { query, withTransaction, fetchAttachment } = vi.hoisted(() => {
  const query = vi.fn<QueryMock>();
  const withTransaction = vi.fn(async <T>(fn: (client: { query: typeof query }) => Promise<T>) => fn({ query }));
  return { query, withTransaction, fetchAttachment: vi.fn() };
});
vi.mock('../services/db.js', () => ({ query, withTransaction }));
vi.mock('../services/encryption.js', () => ({ encrypt: (value: string) => `enc:${value}`, decrypt: (value: string) => value }));
vi.mock('../services/calendarInvitation.js', () => ({ sendCalendarInvitation: vi.fn(), sendCalendarReply: vi.fn() }));
vi.mock('../services/externalCalendarSync.js', () => ({ releaseCalendarSource: vi.fn(), scheduleCalendarSource: vi.fn(), stopCalendarSource: vi.fn(), syncCalendarSource: vi.fn() }));
vi.mock('../services/externalCalendarWriteback.js', () => ({ pushEventToRemote: vi.fn(async () => ({ skipped: true })) }));
vi.mock('../middleware/auth.js', () => ({ requireAuth: (req: { session?: { userId?: string } }, _res: unknown, next: () => void) => { req.session = { userId: 'user-1' }; next(); } }));
vi.mock('../services/hostValidation.js', () => ({ validateHost: vi.fn(async () => null) }));
vi.mock('../services/connectionPolicy.js', () => ({ getConnectionPolicy: vi.fn(async () => ({ allowPrivateHosts: false })) }));
vi.mock('../index.js', () => ({ imapManager: { fetchAttachment } }));

import express from 'express';
import calendarRouter from './calendar.js';

const REPLY = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Apple Inc.//Mac OS X//EN', 'METHOD:REPLY',
  'BEGIN:VEVENT', 'UID:meeting-1', 'SEQUENCE:0', 'DTSTAMP:20261021T100000Z',
  'DTSTART:20261022T090000Z', 'DTEND:20261022T100000Z', 'SUMMARY:Planning',
  'ORGANIZER:mailto:me@example.test', 'ATTENDEE;CN=Max;PARTSTAT=DECLINED:mailto:max@example.test',
  'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');
const OWN_EVENT = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'BEGIN:VEVENT', 'UID:meeting-1', 'DTSTAMP:20261020T100000Z',
  'DTSTART:20261022T090000Z', 'DTEND:20261022T100000Z', 'SUMMARY:Planning',
  'ATTENDEE;PARTSTAT=ACCEPTED:mailto:anna@example.test', 'ATTENDEE:mailto:max@example.test',
  'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');

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
afterAll(async () => { await new Promise((resolve) => server.close(resolve)); });
beforeEach(() => { query.mockReset(); fetchAttachment.mockReset(); });

describe('a received reply in the mail', () => {
  it('is applied to the organizer event and described instead of failing to load', async () => {
    fetchAttachment.mockResolvedValue(Buffer.from(REPLY, 'utf8'));
    query.mockImplementation(async (sql: string) => {
      if (sql.includes('FROM messages m JOIN email_accounts')) return { rows: [{ raw_ical: null, account_id: 'acct-1', uid: 9, folder: 'INBOX', attachments: JSON.stringify([{ part: '2', filename: 'reply.ics', type: 'text/calendar', size: 400 }]) }] };
      if (sql.includes('SELECT * FROM email_accounts')) return { rows: [{ id: 'acct-1' }] };
      if (sql.includes('FROM calendar_events e JOIN calendars')) return { rows: [{ id: 'event-1', uid: 'meeting-1', raw_ical: OWN_EVENT, calendar_id: 'cal-1', source: 'local', read_only: false, external_url: null, user_id: 'user-1' }] };
      if (sql.startsWith('UPDATE calendar_events')) return { rows: [{ id: 'event-1' }] };
      return { rows: [] };
    });
    const response = await fetch(`${base}/api/calendar/invitations/message-reply`);
    expect(response.status).toBe(200);
    const body = await response.json() as { invitation: Record<string, unknown> };
    expect(body.invitation).toMatchObject({ method: 'REPLY', summary: 'Planning', recordedInCalendar: true, attendees: [{ email: 'max@example.test', partstat: 'DECLINED' }] });
    const update = query.mock.calls.find(([sql]) => sql.startsWith('UPDATE calendar_events'));
    expect(String(update?.[1]?.[0])).toMatch(/PARTSTAT=DECLINED:mailto:max@example\.test/);
  });
});

describe('GET /events/:id/attendees', () => {
  it("reports each attendee's answer for the series an occurrence belongs to", async () => {
    query.mockResolvedValue({ rows: [{ raw_ical: OWN_EVENT }] });
    const response = await fetch(`${base}/api/calendar/events/event-1@20261022T090000/attendees`);
    expect(await response.json()).toEqual({ attendees: [
      { email: 'anna@example.test', name: null, partstat: 'ACCEPTED' },
      { email: 'max@example.test', name: null, partstat: 'NEEDS-ACTION' },
    ] });
    expect(query.mock.calls[0][1]).toEqual(['event-1', 'user-1']);
  });

  it('is 404 for an event the user does not own', async () => {
    query.mockResolvedValue({ rows: [] });
    expect((await fetch(`${base}/api/calendar/events/foreign/attendees`)).status).toBe(404);
  });
});
