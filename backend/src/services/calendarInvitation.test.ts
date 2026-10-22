import { describe, expect, it, vi } from 'vitest';
import type {
  createAccountSmtpTransport as createAccountSmtpTransportContract,
  SmtpTransportLike,
} from './smtpTransport.js';

type CreateAccountSmtpTransport = typeof createAccountSmtpTransportContract;
type SmtpSendMailResult = Awaited<ReturnType<NonNullable<SmtpTransportLike['sendMail']>>>;

interface CalendarInvitationMail {
  from: string;
  to: string;
  subject: string;
  text: string;
  attachments: [{ filename: string; content: string; contentType: string }];
}

type SendInvitationMail = (mail: CalendarInvitationMail) => Promise<SmtpSendMailResult>;
type VerifyTransport = () => Promise<unknown>;

function createMailTransportMock() {
  const sendMail = vi.fn<SendInvitationMail>().mockResolvedValue({
    envelope: { from: 'organizer@example.test', to: ['guest@example.test'] },
    messageId: 'test-message-id',
    accepted: [],
    rejected: [],
    pending: [],
    response: '250 OK',
    envelopeTime: 0,
    messageTime: 0,
    messageSize: 0,
  });
  const verify = vi.fn<VerifyTransport>().mockResolvedValue({});
  return { sendMail, transport: { sendMail, verify } };
}

const { createAccountSmtpTransport } = vi.hoisted(() => ({
  createAccountSmtpTransport: vi.fn<CreateAccountSmtpTransport>(),
}));
vi.mock('./smtpTransport.js', () => ({ createAccountSmtpTransport }));

import { invitationReplyIcal, sendCalendarInvitation } from './calendarInvitation.js';
import { parseInboundCalendarInvitation } from './inboundCalendarInvitation.js';

describe('sendCalendarInvitation', () => {
  it('sends a METHOD:REQUEST iCalendar attachment from the selected account', async () => {
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test', sender_name: 'Organizer' }, transport });

    await sendCalendarInvitation({
      account: { id: 'account-1', email_address: 'organizer@example.test' },
      attendees: ['guest@example.test'], summary: 'Planning', uid: 'event-1',
      startsAt: new Date('2026-09-01T09:00:00.000Z'), endsAt: new Date('2026-09-01T10:00:00.000Z'),
    });

    expect(sendMail).toHaveBeenCalledWith(expect.objectContaining({
      from: 'Organizer <organizer@example.test>', to: 'guest@example.test', subject: 'Invitation: Planning',
      attachments: [expect.objectContaining({ contentType: 'text/calendar; charset=utf-8; method=REQUEST' })],
    }));
    expect(sendMail.mock.calls[0][0].attachments[0].content).toContain('METHOD:REQUEST');
    // Long content lines are folded (RFC 5545 §3.1); unfold before matching.
    expect(sendMail.mock.calls[0][0].attachments[0].content.replace(/\r\n[ \t]/g, '')).toContain('ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:guest@example.test');
  });

  it('escapes lone carriage returns in invitation text to prevent iCalendar property injection', async () => {
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test' }, transport });

    await sendCalendarInvitation({
      account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Planning',
      description: 'Details\rX-INJECTED: true', uid: 'event-4',
      startsAt: new Date('2026-09-01T09:00:00.000Z'), endsAt: new Date('2026-09-01T10:00:00.000Z'),
    });

    const content = sendMail.mock.calls[0][0].attachments[0].content;
    expect(content).toContain('DESCRIPTION:Details\\nX-INJECTED: true');
    expect(content).not.toContain('\rX-INJECTED: true');
  });

  it('uses DATE values for all-day invitations', async () => {
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test', sender_name: 'Organizer' }, transport });

    await sendCalendarInvitation({ account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Holiday', uid: 'event-2', allDay: true, startsAt: new Date('2026-09-01T00:00:00.000Z'), endsAt: new Date('2026-09-02T00:00:00.000Z') });

    expect(sendMail.mock.calls[0][0].attachments[0].content).toContain('DTSTART;VALUE=DATE:20260901');
    expect(sendMail.mock.calls[0][0].attachments[0].content).toContain('DTEND;VALUE=DATE:20260902');
  });

  it('marks all-day invitations and replies all-day for Exchange, and leaves timed ones alone', async () => {
    // Microsoft 365 turns a bare VALUE=DATE event into a timed one (midnight UTC), which a Mac or
    // Outlook on that mailbox then shows running into a third day.
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test' }, transport });
    await sendCalendarInvitation({ account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Offsite', uid: 'event-3', allDay: true, startsAt: new Date('2026-10-22T00:00:00.000Z'), endsAt: new Date('2026-10-24T00:00:00.000Z') });
    await sendCalendarInvitation({ account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Call', uid: 'event-4', startsAt: new Date('2026-10-22T09:00:00.000Z'), endsAt: new Date('2026-10-22T10:00:00.000Z') });
    const [allDay, timed] = sendMail.mock.calls.map(call => String(call[0].attachments[0].content));
    expect(allDay).toContain('\r\nX-MICROSOFT-CDO-ALLDAYEVENT:TRUE\r\n');
    expect(allDay).toContain('DTEND;VALUE=DATE:20261024');
    expect(timed).not.toContain('X-MICROSOFT-CDO-ALLDAYEVENT');
    const reply = invitationReplyIcal({ attendeeEmail: 'me@example.test', organizerEmail: 'organizer@example.test', response: 'ACCEPTED', uid: 'event-3', summary: 'Offsite', allDay: true, startsAt: '2026-10-22T00:00:00.000Z', endsAt: '2026-10-24T00:00:00.000Z' });
    expect(reply).toContain('\r\nX-MICROSOFT-CDO-ALLDAYEVENT:TRUE\r\n');
  });

  it('folds long invitation properties without splitting UTF-8 characters', async () => {
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test' }, transport });

    await sendCalendarInvitation({
      account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Release ' + 'ż'.repeat(40),
      uid: 'event-5', startsAt: new Date('2026-09-01T00:00:00.000Z'), endsAt: new Date('2026-09-02T00:00:00.000Z'),
    });

    const content = sendMail.mock.calls[0][0].attachments[0].content;
    expect(content).toContain('SUMMARY:Release ');
    expect(content).toContain('\r\n ');
    for (const line of content.split('\r\n')) expect(Buffer.byteLength(line, 'utf8')).toBeLessThanOrEqual(75);
  });

  it('marks invitation revisions and cancellations with their iCalendar method and sequence', async () => {
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test', sender_name: 'Organizer' }, transport });

    await sendCalendarInvitation({
      account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Cancelled planning', uid: 'event-3',
      startsAt: new Date('2026-09-01T09:00:00.000Z'), endsAt: new Date('2026-09-01T10:00:00.000Z'), method: 'CANCEL', sequence: 2,
    });

    const content = sendMail.mock.calls[0][0].attachments[0].content;
    expect(content).toContain('METHOD:CANCEL');
    expect(content).toContain('SEQUENCE:2');
    expect(content).toContain('STATUS:CANCELLED');
    expect(content.match(/^STATUS:CANCELLED$/gm)).toHaveLength(1);
  });

  it('round-trips generated cancellations through the inbound parser', async () => {
    const { sendMail, transport } = createMailTransportMock();
    createAccountSmtpTransport.mockResolvedValue({ account: { email_address: 'organizer@example.test' }, transport });

    await sendCalendarInvitation({
      account: { id: 'account-1' }, attendees: ['guest@example.test'], summary: 'Cancelled planning', uid: 'event-round-trip',
      startsAt: new Date('2026-09-01T09:00:00.000Z'), endsAt: new Date('2026-09-01T10:00:00.000Z'), method: 'CANCEL', sequence: 2,
    });

    expect(parseInboundCalendarInvitation(sendMail.mock.calls[0][0].attachments[0].content)).toMatchObject({
      method: 'CANCEL', state: 'cancelled', uid: 'event-round-trip', sequence: 2,
      organizer: 'mailto:organizer@example.test', summary: 'Cancelled planning',
    });
  });
});