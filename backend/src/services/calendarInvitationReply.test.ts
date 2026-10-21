import { describe, expect, it } from 'vitest';
import { invitationReplyIcal } from './calendarInvitation.js';

describe('invitationReplyIcal', () => {
  const base = {
    attendeeEmail: 'me@example.test', organizerEmail: 'boss@example.test', uid: 'meeting-1', sequence: 2,
    summary: 'Planning; Q4, final', startsAt: '2026-09-11T13:00:00Z', endsAt: '2026-09-11T14:00:00Z',
  };

  it('builds an RFC 5546 REPLY carrying only the answering attendee', () => {
    const ical = invitationReplyIcal({ ...base, response: 'TENTATIVE' });
    expect(ical).toContain('METHOD:REPLY');
    expect(ical).toContain('UID:meeting-1');
    expect(ical).toContain('SEQUENCE:2');
    expect(ical).toContain('ORGANIZER:mailto:boss@example.test');
    expect(ical).toContain('ATTENDEE;PARTSTAT=TENTATIVE:mailto:me@example.test');
    expect(ical.match(/^ATTENDEE/gm)).toHaveLength(1);
    expect(ical).toContain('SUMMARY:Planning\\; Q4\\, final');
    expect(ical).toContain('DTSTART:20260911T130000Z');
    expect(ical).not.toContain('RECURRENCE-ID');
    expect(ical.split('\r\n').every(line => Buffer.byteLength(line, 'utf8') <= 75)).toBe(true);
  });

  it('answers one occurrence of a series and all-day events by date', () => {
    const ical = invitationReplyIcal({ ...base, response: 'DECLINED', recurrenceId: '20260918T130000Z', allDay: true, startsAt: '2026-09-11T00:00:00Z', endsAt: '2026-09-12T00:00:00Z' });
    expect(ical).toContain('RECURRENCE-ID:20260918T130000Z');
    expect(ical).toContain('DTSTART;VALUE=DATE:20260911');
    expect(ical).toContain('ATTENDEE;PARTSTAT=DECLINED:mailto:me@example.test');
  });
});
