import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./db.js', () => ({ query: vi.fn() }));
vi.mock('./externalCalendarWriteback.js', () => ({ pushEventToRemote: vi.fn(async () => ({ skipped: true })) }));

import { query as __mock_query } from './db.js';
import { applyInvitationReply, applyReplyToIcal, attendeeStatuses, parseInboundCalendarReply } from './calendarReplies.js';

const query = vi.mocked(__mock_query);

const REPLY = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Apple Inc.//Mac OS X//EN', 'METHOD:REPLY',
  'BEGIN:VEVENT', 'UID:meeting-1', 'SEQUENCE:1', 'DTSTAMP:20261021T100000Z',
  'DTSTART:20261022T090000Z', 'DTEND:20261022T100000Z', 'SUMMARY:Planning',
  'ORGANIZER:mailto:me@example.test',
  'ATTENDEE;CN=Max Mustermann;PARTSTAT=ACCEPTED:mailto:Max@Example.test',
  'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');

const OWN_EVENT = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Inboxora//DAV Hub//EN',
  'BEGIN:VEVENT', 'UID:meeting-1', 'DTSTAMP:20261020T100000Z',
  'DTSTART:20261022T090000Z', 'DTEND:20261022T100000Z', 'SUMMARY:Planning',
  'ATTENDEE:mailto:max@example.test', 'ATTENDEE:mailto:other@example.test',
  'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');

describe('parseInboundCalendarReply', () => {
  it('reads the answering attendee and the event it answers', () => {
    expect(parseInboundCalendarReply(REPLY)).toMatchObject({
      method: 'REPLY', uid: 'meeting-1', sequence: 1, summary: 'Planning', allDay: false,
      attendees: [{ email: 'max@example.test', name: 'Max Mustermann', partstat: 'ACCEPTED' }],
    });
  });

  it('reads all-day dates as calendar days, whatever the server timezone', () => {
    const allDay = REPLY.replace('DTSTART:20261022T090000Z', 'DTSTART;VALUE=DATE:20261022').replace('DTEND:20261022T100000Z', 'DTEND;VALUE=DATE:20261024');
    expect(parseInboundCalendarReply(allDay)).toMatchObject({ allDay: true, startsAt: '2026-10-22T00:00:00.000Z', endsAt: '2026-10-24T00:00:00.000Z' });
  });

  it('ignores anything that is not a reply', () => {
    expect(parseInboundCalendarReply(REPLY.replace('METHOD:REPLY', 'METHOD:REQUEST'))).toBeNull();
    expect(parseInboundCalendarReply('not a calendar')).toBeNull();
  });
});

describe('recording a reply', () => {
  beforeEach(() => { query.mockReset(); });

  it('sets the attendee PARTSTAT and leaves the others alone', () => {
    const raw = applyReplyToIcal(OWN_EVENT, parseInboundCalendarReply(REPLY)!);
    expect(attendeeStatuses(raw)).toEqual([
      { email: 'max@example.test', name: null, partstat: 'ACCEPTED' },
      { email: 'other@example.test', name: null, partstat: 'NEEDS-ACTION' },
    ]);
    // Applying the same reply again changes nothing.
    expect(applyReplyToIcal(raw!, parseInboundCalendarReply(REPLY)!)).toBeNull();
  });

  it('updates only events this user sent invitations from, guarded against concurrent edits', async () => {
    query.mockImplementation(async (sql: string) => (sql.includes('FROM calendar_events e JOIN calendars')
      ? { rows: [{ id: 'event-1', uid: 'meeting-1', raw_ical: OWN_EVENT, calendar_id: 'cal-1', source: 'local', read_only: false, external_url: null, user_id: 'user-1' }] }
      : { rows: [{ id: 'event-1' }] }));
    expect(await applyInvitationReply('user-1', parseInboundCalendarReply(REPLY)!)).toBe(1);
    const [select, update] = query.mock.calls;
    expect(select[0]).toContain('e.invite_account_id IS NOT NULL');
    expect(select[1]).toEqual(['user-1', 'meeting-1']);
    expect(update[0]).toContain('raw_ical = $4');
    expect(String(update[1]?.[0])).toContain('PARTSTAT=ACCEPTED');
  });
});
