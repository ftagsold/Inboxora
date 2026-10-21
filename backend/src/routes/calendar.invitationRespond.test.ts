// Accept / maybe / decline a received invitation directly from the mail.
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { listeningPort } from '../test/net.js';
import type { Server } from 'node:http';
import type { NextFunction, Request, Response } from 'express';
import type { DbClient } from '../services/db.js';
import 'express-async-errors';

type QueryMock = DbClient['query'];

const { query, withTransaction, sendCalendarReply } = vi.hoisted(() => {
  const query = vi.fn<QueryMock>();
  const withTransaction = vi.fn(async <T>(fn: (client: { query: typeof query }) => Promise<T>) => fn({ query }));
  return { query, withTransaction, sendCalendarReply: vi.fn(async () => {}) };
});
vi.mock('../services/db.js', () => ({ query, withTransaction }));
vi.mock('../services/encryption.js', () => ({ encrypt: (value: string) => `enc:${value}`, decrypt: (value: string) => value }));
vi.mock('../services/calendarInvitation.js', () => ({ sendCalendarInvitation: vi.fn(), sendCalendarReply }));
vi.mock('../services/externalCalendarSync.js', () => ({ releaseCalendarSource: vi.fn(), scheduleCalendarSource: vi.fn(), stopCalendarSource: vi.fn(), syncCalendarSource: vi.fn() }));
vi.mock('../services/externalCalendarWriteback.js', () => ({ pushEventToRemote: vi.fn(async () => {}) }));
vi.mock('../middleware/auth.js', () => ({ requireAuth: (req: { session?: { userId?: string } }, _res: unknown, next: () => void) => { req.session = { userId: 'user-1' }; next(); } }));
vi.mock('../services/hostValidation.js', () => ({ validateHost: vi.fn(async () => null) }));
vi.mock('../services/connectionPolicy.js', () => ({ getConnectionPolicy: vi.fn(async () => ({ allowPrivateHosts: false })) }));
vi.mock('../index.js', () => ({ imapManager: { fetchAttachment: vi.fn() } }));

import express from 'express';
import calendarRouter from './calendar.js';

const MESSAGE_ID = '503d6b49-2095-4e97-8e59-0cb643e71539';
const ACCOUNT_ID = 'c9f370a0-936e-4c22-8b8b-20952ffcf1e1';
const CALENDAR_ID = '7a1b2c3d-0000-4000-8000-000000000001';

const INVITATION = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Example//EN', 'METHOD:REQUEST',
  'BEGIN:VEVENT', 'UID:meeting-1', 'SEQUENCE:2',
  'DTSTAMP:20260911T120000Z', 'DTSTART:20260911T130000Z', 'DTEND:20260911T140000Z',
  'ORGANIZER:mailto:boss@example.test', 'SUMMARY:Planning',
  'ATTENDEE;ROLE=REQ-PARTICIPANT:mailto:Me@Example.test',
  'ATTENDEE;ROLE=REQ-PARTICIPANT:mailto:other@example.test',
  'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');

let server: Server;
let base = '';
let stored: unknown[] | null = null;

beforeAll(async () => {
  const app = express();
  app.use(express.json());
  app.use('/api/calendar', calendarRouter);
  app.use((error: Error, _req: Request, res: Response, next: NextFunction) => { void next; return res.status(500).json({ error: error.message }); });
  await new Promise((resolve) => { server = app.listen(0, resolve); });
  base = `http://127.0.0.1:${listeningPort(server)}`;
});
afterAll(async () => { await new Promise((resolve) => server.close(resolve)); });

beforeEach(() => {
  stored = null;
  sendCalendarReply.mockReset().mockResolvedValue(undefined);
  query.mockReset().mockImplementation(async (sql: string, params?: unknown[]) => {
    if (sql.includes('FROM messages m JOIN email_accounts')) return { rows: [{ raw_ical: INVITATION, account_id: ACCOUNT_ID, uid: 42, folder: 'INBOX', attachments: '[]' }] };
    if (sql.includes('FROM calendars WHERE id = $1')) return { rows: [{ id: CALENDAR_ID, user_id: 'user-1', source: 'local', read_only: false, external_url: null }] };
    if (sql.includes('UNION ALL')) return { rows: [{ email: 'me@example.test', kind: 'account' }] };
    if (sql.includes('SELECT * FROM email_accounts')) return { rows: [{ id: ACCOUNT_ID, email_address: 'me@example.test' }] };
    if (sql.includes('INSERT INTO calendar_invitation_responses')) { stored = params ?? null; return { rows: [] }; }
    if (sql.includes('INSERT INTO calendar_events')) return { rows: [{ id: 'event-1' }] };
    return { rows: [] };
  });
});

const respond = (body: unknown) => fetch(`${base}/api/calendar/invitations/${MESSAGE_ID}/respond`, {
  method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
});

describe('POST /api/calendar/invitations/:messageId/respond', () => {
  it('accepts: replies to the organizer as the invited address, records it and adds the event with PARTSTAT', async () => {
    const response = await respond({ response: 'accepted', calendarId: CALENDAR_ID });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ response: 'ACCEPTED', localEvent: { calendarId: CALENDAR_ID } });
    expect(sendCalendarReply).toHaveBeenCalledWith(expect.objectContaining({
      attendeeEmail: 'me@example.test', organizerEmail: 'boss@example.test', response: 'ACCEPTED', uid: 'meeting-1', sequence: 2,
    }));
    expect(stored).toEqual(['user-1', 'meeting-1', '', 'boss@example.test', 2, 'ACCEPTED']);
    const insert = query.mock.calls.find(([sql]) => sql.includes('INSERT INTO calendar_events'));
    expect(String(insert?.[1]?.[3])).toMatch(/ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:Me@Example\.test/i);
    // Re-answering the same revision refreshes the stored copy.
    expect(insert?.[0]).toContain('invitation_sequence <= EXCLUDED.invitation_sequence');
  });

  it('declines: replies and removes only the copy this message added', async () => {
    const response = await respond({ response: 'DECLINED' });
    expect(response.status).toBe(200);
    expect(sendCalendarReply).toHaveBeenCalledWith(expect.objectContaining({ response: 'DECLINED' }));
    const removal = query.mock.calls.find(([sql]) => sql.startsWith('DELETE FROM calendar_events'));
    expect(removal?.[0]).toContain('invite_account_id IS NULL');
    expect(removal?.[1]).toEqual(['user-1', MESSAGE_ID]);
    expect(query.mock.calls.some(([sql]) => sql.includes('INSERT INTO calendar_events'))).toBe(false);
  });

  it('records nothing and changes no calendar when the reply cannot be sent', async () => {
    sendCalendarReply.mockRejectedValueOnce(new Error('SMTP down'));
    vi.spyOn(console, 'error').mockImplementationOnce(() => {});
    const response = await respond({ response: 'TENTATIVE', calendarId: CALENDAR_ID });
    expect(response.status).toBe(502);
    expect(stored).toBeNull();
    expect(query.mock.calls.some(([sql]) => /INSERT INTO calendar_events|DELETE FROM calendar_events/.test(sql))).toBe(false);
  });

  it('rejects an unknown answer without sending anything', async () => {
    const response = await respond({ response: 'MAYBE-LATER' });
    expect(response.status).toBe(400);
    expect(sendCalendarReply).not.toHaveBeenCalled();
  });

  it('needs a target calendar to accept', async () => {
    const response = await respond({ response: 'ACCEPTED' });
    expect(response.status).toBe(400);
    expect(sendCalendarReply).not.toHaveBeenCalled();
  });
});

describe('GET /api/calendar/invitations/:messageId response', () => {
  it('reports the stored answer for the current revision', async () => {
    const original = query.getMockImplementation();
    query.mockImplementation(async (sql: string, params?: unknown[]) => (sql.includes('FROM calendar_invitation_responses')
      ? (expect(params).toEqual(['user-1', 'meeting-1', '', 'boss@example.test', 2]), { rows: [{ response: 'TENTATIVE' }] })
      : original!(sql, params)));
    const response = await fetch(`${base}/api/calendar/invitations/${MESSAGE_ID}`);
    const body = await response.json() as { invitation: { response: string } };
    expect(body.invitation.response).toBe('TENTATIVE');
    expect(body.invitation).not.toHaveProperty('accountId');
  });
});
