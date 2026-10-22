import { descriptionContentLines } from '../utils/richText.js';

function escapeICalendarText(value: unknown): string {
  return String(value || '')
    .replaceAll('\\', '\\\\')
    .replaceAll('\r\n', '\n')
    .replaceAll('\r', '\n')
    .replaceAll('\n', '\\n')
    .replaceAll(';', '\\;')
    .replaceAll(',', '\\,');
}

function formatICalendarDate(value: Date): string {
  return value.toISOString().replace(/\.\d{3}Z$/, 'Z').replaceAll('-', '').replaceAll(':', '');
}

function formatInvitationDate(value: string | number | Date, allDay: boolean): string {
  const date = value instanceof Date ? value : new Date(value);
  return allDay ? date.toISOString().slice(0, 10).replaceAll('-', '') : formatICalendarDate(date);
}

function foldICalendarLine(line: string) {
  const chunks: string[] = [];
  let chunk = '';
  let limit = 75;
  for (const character of line) {
    if (Buffer.byteLength(chunk + character, 'utf8') > limit && chunk) {
      chunks.push(chunk);
      chunk = character;
      limit = 74;
    } else chunk += character;
  }
  chunks.push(chunk);
  return chunks.join('\r\n ');
}

/** One invitation to render and send. */
interface InvitationInput {
  uid: string;
  summary?: string | null;
  description?: string | null;
  location?: string | null;
  organizerEmail?: string | null;
  attendees: string[];
  startsAt: Date | string | number;
  endsAt: Date | string | number;
  allDay?: boolean;
  method?: string;
  sequence?: number;
}

/** The account slice the invitation is sent from. */
interface InvitationAccount { id?: string; email_address?: string | null; name?: string | null; [key: string]: unknown }


// Exchange / Microsoft 365 does not treat a VALUE=DATE event as all-day on its own: it converts
// the dates to times (midnight UTC, i.e. 02:00 in Berlin) and the event then runs into a third
// day for a Mac or Outlook reading that mailbox. These Microsoft properties mark the event
// all-day for Exchange; every other calendar ignores them.
function allDayHints(allDay: boolean): string[] {
  return allDay ? ['X-MICROSOFT-CDO-ALLDAYEVENT:TRUE', 'X-MICROSOFT-MSNCALENDAR-ALLDAYEVENT:TRUE'] : [];
}

function invitationIcal({ uid, summary, description, location, organizerEmail, attendees, startsAt, endsAt, allDay = false, method, sequence }: InvitationInput) {
  const lines = [
    'BEGIN:VCALENDAR',
    'VERSION:2.0',
    'PRODID:-//Inboxora//Calendar//EN',
    `METHOD:${method}`,
    'BEGIN:VEVENT',
    `UID:${uid}`,
    `SEQUENCE:${sequence}`,
    `DTSTAMP:${formatICalendarDate(new Date())}`,
    `DTSTART${allDay ? ';VALUE=DATE' : ''}:${formatInvitationDate(startsAt, allDay)}`,
    `DTEND${allDay ? ';VALUE=DATE' : ''}:${formatInvitationDate(endsAt, allDay)}`,
    `ORGANIZER:mailto:${organizerEmail}`,
    ...allDayHints(allDay),
  ];
  if (method === 'CANCEL') lines.push('STATUS:CANCELLED');
  if (summary) lines.push(`SUMMARY:${escapeICalendarText(summary)}`);
  lines.push(...descriptionContentLines(description, escapeICalendarText));
  if (location) lines.push(`LOCATION:${escapeICalendarText(location)}`);
  // PARTSTAT/RSVP ask the attendee's calendar to answer, which comes back as a METHOD:REPLY.
  for (const attendee of attendees) lines.push(`ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:${attendee}`);
  lines.push('END:VEVENT', 'END:VCALENDAR', '');
  return lines.map(foldICalendarLine).join('\r\n');
}

export async function sendCalendarInvitation({ account, attendees, summary, description = null, location = null, uid, startsAt, endsAt, allDay = false, method = 'REQUEST', sequence = 0 }: InvitationInput & { account: InvitationAccount }) {
  const { createAccountSmtpTransport } = await import('./smtpTransport.js');
  const smtp = await createAccountSmtpTransport(account);
  if (smtp.error) throw Object.assign(new Error(smtp.error), { status: smtp.status });
  // createAccountSmtpTransport returns either { account, transport } or { status, error }; the guard above
  // catches the error branch, so this is unreachable - it exists so the compiler can see the same fact.
  if (!smtp.account || !smtp.transport) throw Object.assign(new Error(smtp.error || 'SMTP transport unavailable'), { status: smtp.status });
  const sendingAccount = smtp.account;
  const fromEmail = sendingAccount.email_address;
  const fromName = sendingAccount.sender_name || sendingAccount.name || fromEmail;
  const content = invitationIcal({ uid, summary, description, location, organizerEmail: fromEmail, attendees, startsAt, endsAt, allDay, method, sequence });
  await smtp.transport.sendMail({
    from: `${fromName} <${fromEmail}>`,
    to: attendees.join(', '),
    subject: `Invitation: ${summary || 'Meeting'}`,
    text: `${fromName} invited you to ${summary || 'a meeting'}.`,
    attachments: [{ filename: 'invitation.ics', content, contentType: `text/calendar; charset=utf-8; method=${method}` }],
  });
}

export type InvitationResponse = 'ACCEPTED' | 'TENTATIVE' | 'DECLINED';

/** One reply to a received invitation (RFC 5546 METHOD:REPLY). */
interface ReplyInput {
  account: InvitationAccount;
  /** The address this user was invited as; it must be the ATTENDEE the organizer knows. */
  attendeeEmail: string;
  organizerEmail: string;
  response: InvitationResponse;
  uid: string;
  recurrenceId?: string | null;
  sequence?: number;
  summary?: string | null;
  startsAt: Date | string | number;
  endsAt: Date | string | number;
  allDay?: boolean;
}

const REPLY_SUBJECT: Record<InvitationResponse, string> = { ACCEPTED: 'Accepted', TENTATIVE: 'Tentative', DECLINED: 'Declined' };

export function invitationReplyIcal({ attendeeEmail, organizerEmail, response, uid, recurrenceId, sequence = 0, summary, startsAt, endsAt, allDay = false }: Omit<ReplyInput, 'account'>) {
  const lines = [
    'BEGIN:VCALENDAR',
    'VERSION:2.0',
    'PRODID:-//Inboxora//Calendar//EN',
    'METHOD:REPLY',
    'BEGIN:VEVENT',
    `UID:${uid}`,
    `SEQUENCE:${sequence}`,
    `DTSTAMP:${formatICalendarDate(new Date())}`,
  ];
  // The parser normalises RECURRENCE-ID to a UTC basic-format timestamp.
  if (recurrenceId) lines.push(`RECURRENCE-ID:${recurrenceId}`);
  lines.push(
    `DTSTART${allDay ? ';VALUE=DATE' : ''}:${formatInvitationDate(startsAt, allDay)}`,
    `DTEND${allDay ? ';VALUE=DATE' : ''}:${formatInvitationDate(endsAt, allDay)}`,
    `ORGANIZER:mailto:${organizerEmail}`,
    `ATTENDEE;PARTSTAT=${response}:mailto:${attendeeEmail}`,
    ...allDayHints(allDay),
  );
  if (summary) lines.push(`SUMMARY:${escapeICalendarText(summary)}`);
  lines.push('END:VEVENT', 'END:VCALENDAR', '');
  return lines.map(foldICalendarLine).join('\r\n');
}

/** Send the user's accept / tentative / decline answer to the organizer. */
export async function sendCalendarReply(input: ReplyInput) {
  const { createAccountSmtpTransport } = await import('./smtpTransport.js');
  const smtp = await createAccountSmtpTransport(input.account);
  if (smtp.error) throw Object.assign(new Error(smtp.error), { status: smtp.status });
  if (!smtp.account || !smtp.transport) throw Object.assign(new Error(smtp.error || 'SMTP transport unavailable'), { status: smtp.status });
  const fromName = smtp.account.sender_name || smtp.account.name || input.attendeeEmail;
  const subject = `${REPLY_SUBJECT[input.response]}: ${input.summary || 'Meeting'}`;
  await smtp.transport.sendMail({
    from: `${fromName} <${input.attendeeEmail}>`,
    to: input.organizerEmail,
    subject,
    text: `${fromName} ${REPLY_SUBJECT[input.response].toLowerCase()} the invitation "${input.summary || 'Meeting'}".`,
    // icalEvent adds the text/calendar; method=REPLY alternative that calendar servers process.
    icalEvent: { method: 'REPLY', content: invitationReplyIcal(input) },
  });
}
