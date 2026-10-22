import ICAL from 'ical.js';
import { query } from './db.js';
import { pushEventToRemote } from './externalCalendarWriteback.js';

// Answers to invitations this user sent: an RFC 5546 METHOD:REPLY carries the attendee's
// PARTSTAT. It is recorded in the organizer's own event (the ATTENDEE line of its iCalendar,
// which stays the single source of truth that CalDAV clients also read).

export type Partstat = 'ACCEPTED' | 'TENTATIVE' | 'DECLINED' | 'NEEDS-ACTION' | 'DELEGATED';

export interface InboundCalendarReply {
  method: 'REPLY';
  uid: string;
  recurrenceId: string | null;
  sequence: number;
  summary: string | null;
  startsAt: string | null;
  endsAt: string | null;
  allDay: boolean;
  attendees: Array<{ email: string; name: string | null; partstat: Partstat }>;
}

const PARTSTATS = new Set<Partstat>(['ACCEPTED', 'TENTATIVE', 'DECLINED', 'NEEDS-ACTION', 'DELEGATED']);
const MAX_ICAL_BYTES = 1024 * 1024;

const address = (value: unknown) => String(value ?? '').replace(/^mailto:/i, '').trim().toLowerCase();

export function parseInboundCalendarReply(raw: unknown): InboundCalendarReply | null {
  if (typeof raw !== 'string' || !raw.trim() || Buffer.byteLength(raw, 'utf8') > MAX_ICAL_BYTES) return null;
  let calendar: ICAL.Component;
  try { calendar = new ICAL.Component(ICAL.parse(raw)); } catch { return null; }
  if (String(calendar.getFirstPropertyValue('method') || '').toUpperCase() !== 'REPLY') return null;
  const vevent = calendar.getFirstSubcomponent('vevent');
  const uid = vevent && String(vevent.getFirstPropertyValue('uid') || '').trim();
  if (!vevent || !uid) return null;
  const attendees = vevent.getAllProperties('attendee').map(property => {
    const partstat = String(property.getParameter('partstat') || 'NEEDS-ACTION').toUpperCase() as Partstat;
    const name = property.getParameter('cn');
    return { email: address(property.getFirstValue()), name: name ? String(name) : null, partstat };
  }).filter(attendee => attendee.email.includes('@') && PARTSTATS.has(attendee.partstat));
  if (!attendees.length) return null;
  const time = (name: string) => {
    const value = vevent.getFirstPropertyValue(name);
    return value instanceof ICAL.Time ? value : null;
  };
  // A DATE value is a calendar day: stored as UTC midnight like every other all-day date,
  // not as midnight in this server's timezone (which toJSDate would give).
  const iso = (value: ICAL.Time) => (value.isDate ? new Date(Date.UTC(value.year, value.month - 1, value.day)) : value.toJSDate()).toISOString();
  const start = time('dtstart');
  const end = time('dtend');
  const recurrence = time('recurrence-id');
  return {
    method: 'REPLY',
    uid,
    recurrenceId: recurrence ? iso(recurrence) : null,
    sequence: Number(vevent.getFirstPropertyValue('sequence') || 0) || 0,
    summary: vevent.getFirstPropertyValue('summary') ? String(vevent.getFirstPropertyValue('summary')) : null,
    startsAt: start ? iso(start) : null,
    endsAt: end ? iso(end) : null,
    allDay: Boolean(start?.isDate),
    attendees,
  };
}

/** Each attendee's PARTSTAT in an event's iCalendar (the series master's ATTENDEE lines). */
export function attendeeStatuses(rawIcal: unknown): Array<{ email: string; name: string | null; partstat: Partstat }> {
  if (typeof rawIcal !== 'string' || !rawIcal.trim()) return [];
  try {
    const calendar = new ICAL.Component(ICAL.parse(rawIcal));
    const vevents = calendar.getAllSubcomponents('vevent');
    const master = vevents.find(vevent => !vevent.hasProperty('recurrence-id')) || vevents[0];
    if (!master) return [];
    return master.getAllProperties('attendee').map(property => {
      const partstat = String(property.getParameter('partstat') || 'NEEDS-ACTION').toUpperCase() as Partstat;
      const name = property.getParameter('cn');
      return { email: address(property.getFirstValue()), name: name ? String(name) : null, partstat: PARTSTATS.has(partstat) ? partstat : 'NEEDS-ACTION' };
    }).filter(attendee => attendee.email.includes('@'));
  } catch {
    return [];
  }
}

/** Set the replying attendees' PARTSTAT in a raw iCalendar. Returns null when nothing changed. */
export function applyReplyToIcal(rawIcal: string, reply: InboundCalendarReply): string | null {
  let calendar: ICAL.Component;
  try { calendar = new ICAL.Component(ICAL.parse(rawIcal)); } catch { return null; }
  const answers = new Map(reply.attendees.map(attendee => [attendee.email, attendee.partstat]));
  let changed = false;
  for (const vevent of calendar.getAllSubcomponents('vevent')) {
    for (const property of vevent.getAllProperties('attendee')) {
      const answer = answers.get(address(property.getFirstValue()));
      if (!answer || String(property.getParameter('partstat') || '').toUpperCase() === answer) continue;
      property.setParameter('partstat', answer);
      changed = true;
    }
  }
  return changed ? calendar.toString() : null;
}

/**
 * Record a reply in the user's own copy of the event it answers: an event this user invited
 * attendees to (invite_account_id set) with the same UID. Idempotent — reading the same reply
 * again changes nothing. Returns how many events were updated.
 */
export async function applyInvitationReply(userId: string, reply: InboundCalendarReply): Promise<number> {
  const events = await query<{ id: string; uid: string; raw_ical: string; calendar_id: string; source: string; read_only: boolean; external_url: string | null; user_id: string }>(
    `SELECT e.id, e.uid, e.raw_ical, e.calendar_id, c.source, c.read_only, c.external_url, c.user_id
       FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id
      WHERE e.user_id = $1 AND e.uid = $2 AND e.invite_account_id IS NOT NULL`,
    [userId, reply.uid],
  );
  let updated = 0;
  for (const event of events.rows) {
    const raw = applyReplyToIcal(event.raw_ical, reply);
    if (!raw) continue;
    const result = await query(
      `UPDATE calendar_events SET raw_ical = $1, etag = gen_random_uuid()::text, updated_at = NOW()
        WHERE id = $2 AND user_id = $3 AND raw_ical = $4 RETURNING id`,
      [raw, event.id, userId, event.raw_ical],
    );
    if (!result.rows[0]) continue;
    updated += 1;
    if (event.source === 'caldav') {
      await pushEventToRemote({ calendar: { id: event.calendar_id, user_id: event.user_id, source: event.source, read_only: event.read_only, external_url: event.external_url }, uid: event.uid, rawIcal: raw, action: 'put' })
        .catch((err: Error) => console.warn('Pushing an invitation reply to the remote calendar failed:', err.message));
    }
  }
  return updated;
}
