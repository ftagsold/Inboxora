import { mergeCalendarResource, truncateSeriesBefore } from '../utils/calendarRecurrence.js';
import type { AttachmentRef, EmailAccountRow } from '../services/imapManager.js';
import ICAL from 'ical.js';
import { parseInboundCalendarInvitation } from '../services/inboundCalendarInvitation.js';
import { parseCalendarEvent } from '../utils/ical.js';
import { descriptionContentLines, normalizeDescription } from '../utils/richText.js';
import { Router } from 'express';
import type { Request, Response, NextFunction } from 'express';
import crypto from 'crypto';
import { query, withTransaction } from '../services/db.js';
import { requireAuth } from '../middleware/auth.js';
import { decrypt, encrypt } from '../services/encryption.js';
import { getConnectionPolicy } from '../services/connectionPolicy.js';
import { releaseCalendarSource, scheduleCalendarSource, stopCalendarSource, syncCalendarSource, parseSourceCollections } from '../services/externalCalendarSync.js';
import type { SourceCollection } from '../services/externalCalendarSync.js';
import { validateSourceUrl, resolveCaldavUrl, clampIntervalSeconds, validatedCollectionHref } from '../services/davSourceUrl.js';
import type { IncomingCollection } from '../services/davSourceUrl.js';
import { pushEventToRemote } from '../services/externalCalendarWriteback.js';
import { sendCalendarInvitation, sendCalendarReply } from '../services/calendarInvitation.js';
import type { InvitationResponse } from '../services/calendarInvitation.js';
import { applyInvitationReply, attendeeStatuses, parseInboundCalendarReply } from '../services/calendarReplies.js';
import { deliverInvitationOutbox, deliverStoredInvitation, invitationActionsForStorage, invitationDeliveryError, resolveInvitationActions } from '../services/calendarInvitationOutbox.js';
import { projectCalendarResources } from '../services/calendarProjectionPool.js';
import { EVENT_COLUMNS, coveragePredicate } from '../services/calendarOccurrences.js';
import { queryString, sessionUserId } from '../utils/query.js';
import { toAppError } from '../utils/errors.js';

const router = Router();
const MAX_EVENT_RANGE_DAYS = 366;
const MAX_EVENT_RANGE_MS = MAX_EVENT_RANGE_DAYS * 24 * 60 * 60 * 1000;
const CONTACT_CALENDAR_ID = 'contacts-birthdays';
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
router.use(requireAuth);

// Calendar selection is opt-in: an absent parameter means "every calendar"
// (the historical behaviour), while an explicitly empty selection means "none".
// Ownership is still enforced by the SQL filter (`c.user_id`/`c.owner_user_id`),
// so a foreign id can only ever match zero rows.
/** Either the requested ids (null = all) or a validation error. */
type CalendarSelection = { ids: string[] | null; error: null } | { ids: null; error: string };

function parseCalendarSelection(raw: unknown): CalendarSelection {
  if (raw === undefined || raw === null) return { ids: null, error: null };
  const parts = (Array.isArray(raw) ? raw : [raw])
    .flatMap(value => String(value).split(','))
    .map(value => value.trim())
    .filter(Boolean);
  const ids: string[] = [];
  for (const id of parts) {
    if (id === CONTACT_CALENDAR_ID) { ids.push(id); continue; }
    if (!UUID_PATTERN.test(id)) return { ids: null, error: 'Invalid calendar id' };
    ids.push(id);
  }
  return { ids: [...new Set(ids)], error: null };
}

function parseEventTimes(body: Record<string, unknown> | null | undefined): { startsAt: Date; endsAt: Date } | null {
  const startsAt = new Date(String(body?.startsAt ?? ''));
  const endsAt = new Date(String(body?.endsAt ?? ''));
  if (Number.isNaN(startsAt.getTime()) || Number.isNaN(endsAt.getTime()) || endsAt <= startsAt) {
    return null;
  }
  return { startsAt, endsAt };
}

/** A calendar event row as the /events reads return it. */
type CalendarEventRow = {
  id: string;
  starts_at: Date | string;
  ends_at: Date | string;
  [key: string]: unknown;
};

/** A contact row carrying its date entries. */
type ContactDateRow = { id?: string; display_name?: string | null; primary_email?: string | null; contact_dates?: unknown; [key: string]: unknown };

function contactDateEvents(contacts: ContactDateRow[], from: Date, to: Date) {
  const events: Array<{ id: string; calendar_id: string; uid: string; summary: string; contact_date_label?: string | null; contact_name?: string | null; starts_at: Date; ends_at: Date; all_day: boolean; contact?: ContactDateRow; date?: { value?: unknown }; [key: string]: unknown }> = [];
  for (const contact of contacts) {
    const dates = [
      ...(Array.isArray(contact.contact_dates) ? contact.contact_dates : []),
      ...(contact.birthday ? [{ label: 'Birthday', value: contact.birthday }] : []),
      ...(contact.anniversary ? [{ label: 'Anniversary', value: contact.anniversary }] : []),
    ];
    const seen = new Set();
    for (const date of dates) {
      if (!date || typeof date !== 'object') continue;
      const label = typeof date.label === 'string' && date.label.trim() ? date.label.trim() : 'Other';
      if (date.value instanceof Date && Number.isNaN(date.value.getTime())) continue;
      const value = date.value instanceof Date ? date.value.toISOString().slice(0, 10) : typeof date.value === 'string' ? date.value.trim() : '';
      if (!/^(?:\d{4}|-)-\d{2}-\d{2}$/.test(value)) continue;
      const key = `${label.toLocaleLowerCase()}\u0000${value}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const [, , month, day] = value.match(/^(\d{4}|-)-(\d{2})-(\d{2})$/);
      for (let year = from.getUTCFullYear(); year <= to.getUTCFullYear(); year++) {
        const startsAt = new Date(Date.UTC(year, Number(month) - 1, Number(day)));
        if (startsAt.getUTCMonth() !== Number(month) - 1 || startsAt.getUTCDate() !== Number(day) || startsAt < from || startsAt >= to) continue;
        const endsAt = new Date(startsAt); endsAt.setUTCDate(endsAt.getUTCDate() + 1);
        const labelKey = crypto.createHash('sha256').update(label).digest('hex').slice(0, 16);
        const dateSlug = value.replaceAll('-', '');
        const id = `contacts-${contact.id}-${labelKey}-${dateSlug}-${year}`;
        events.push({ id, calendar_id: 'contacts-birthdays', uid: id, summary: `${label}: ${contact.display_name || contact.primary_email || 'Contact'}`, contact_date_label: label, contact_name: contact.display_name || contact.primary_email || null, starts_at: startsAt, ends_at: endsAt, all_day: true, calendar_name: 'Contact dates', calendar_color: '#e879f9', source: 'contacts', read_only: true });
      }
    }
  }
  return events;
}

/** Sort key for a stored or projected event; a missing start sorts as invalid, exactly as `new Date(undefined)` did. */
function eventStartTime(value: Date | string | undefined): number {
  return value === undefined ? Number.NaN : new Date(value).getTime();
}

function escapeICalendarText(value: unknown): string {
  return String(value || '')
    .replaceAll('\\', '\\\\')
    .replaceAll('\r\n', '\n')
    .replaceAll('\r', '\n')
    .replaceAll('\n', '\\n')
    .replaceAll(';', '\\;')
    .replaceAll(',', '\\,');
}

function formatICalendarDate(value: Date, allDay: boolean): string {
  const utc = value.toISOString();
  return allDay
    ? utc.slice(0, 10).replaceAll('-', '')
    : utc.replaceAll('-', '').replaceAll(':', '').replace('.000', '');
}

function foldICalendarLine(line: string) {
  const chunks = [];
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

/** The rendered invitation fields `localEventIcal` turns into one VEVENT. */
type LocalEventIcalInput = {
  uid: string;
  summary?: string | null;
  description?: string | null;
  location?: string | null;
  url?: string | null;
  organizer?: string | null;
  attendees?: string[];
  /** Each attendee's recorded answer (PARTSTAT), kept on the ATTENDEE line. */
  partstats?: Map<string, string>;
  startsAt: Date;
  endsAt: Date;
  allDay: boolean;
};

/** The values an invitation request carries, shared by the create and update paths. */
type InvitationFields = {
  calendarId: string;
  invitationAccount: EmailAccountRow;
  normalizedAttendees: string[];
  times: { startsAt: Date; endsAt: Date };
  summary?: string | null;
  description?: string | null;
  location?: string | null;
  url?: string | null;
  organizer?: string | null;
  allDay?: boolean | null;
  timezone?: string | null;
};

/** The outbox delivery result an invitation response reports. */
type InvitationDeliveryStatus = { status?: string | null; lastError?: string | null };

/** The message columns the invitation attachment fallback reads. */
type InvitationMessageRow = {
  account_id: string;
  uid: string | number;
  folder: string;
  attachments?: AttachmentRef[] | string | null;
  raw_ical?: string | null;
};

function localEventIcal({ uid, summary, description, location, url, organizer, attendees = [], partstats, startsAt, endsAt, allDay }: LocalEventIcalInput) {
  const dateParameter = allDay ? ';VALUE=DATE' : '';
  const lines = ['BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Inboxora//DAV Hub//EN', 'BEGIN:VEVENT', `UID:${uid}`, `DTSTAMP:${formatICalendarDate(new Date(), false)}`, `DTSTART${dateParameter}:${formatICalendarDate(startsAt, allDay)}`, `DTEND${dateParameter}:${formatICalendarDate(endsAt, allDay)}`];
  if (summary) lines.push(`SUMMARY:${escapeICalendarText(summary)}`);
  lines.push(...descriptionContentLines(description, escapeICalendarText));
  if (location) lines.push(`LOCATION:${escapeICalendarText(location)}`);
  if (url) lines.push(`URL:${String(url).replace(/[\r\n]/g, '')}`);
  if (organizer) lines.push(`ORGANIZER:mailto:${escapeICalendarText(organizer.replace(/^mailto:/i, ''))}`);
  for (const email of attendees) {
    const partstat = partstats?.get(email.toLowerCase());
    lines.push(`ATTENDEE${partstat ? `;PARTSTAT=${partstat}` : ''}:mailto:${email}`);
  }
  lines.push('END:VEVENT', 'END:VCALENDAR', '');
  return lines.map(foldICalendarLine).join('\r\n');
}

/**
 * The answers attendees already gave, carried into an edited event — unless its time changed,
 * which is a new question (the re-sent invitation asks everyone again).
 */
function preservedPartstats(existing: { raw_ical?: string | null; starts_at?: Date | string | null; ends_at?: Date | string | null }, times: { startsAt: Date; endsAt: Date }) {
  const same = (left: Date | string | null | undefined, right: Date) => left != null && new Date(left).getTime() === right.getTime();
  if (!same(existing.starts_at, times.startsAt) || !same(existing.ends_at, times.endsAt)) return new Map<string, string>();
  return new Map(attendeeStatuses(existing.raw_ical).filter(attendee => attendee.partstat !== 'NEEDS-ACTION').map(attendee => [attendee.email, attendee.partstat]));
}

/**
 * Who an edit of an invited event is sent to. Attendees hear about it again only when what
 * their invitation says changed (title, description, place, time); otherwise only attendees
 * added by this edit are invited. The sequence moves on whenever the stored invitation
 * changes for someone — details, a removed attendee (whose CANCEL carries the next sequence)
 * or a new sender — so a later update always supersedes what attendees hold.
 */
export function invitationUpdatePlan(
  existing: { attendees?: unknown; invite_account_id?: string | null; summary?: string | null; description?: string | null; location?: string | null; starts_at?: Date | string | null; ends_at?: Date | string | null; all_day?: boolean | null },
  next: { attendees: string[]; inviteAccountId: string | null; summary?: string | null; description?: string | null; location?: string | null; startsAt: Date; endsAt: Date; allDay: boolean },
): { bumpSequence: boolean; recipients: string[] } {
  const previous = Array.isArray(existing.attendees) ? existing.attendees.map(email => String(email).toLowerCase()) : [];
  const hadInvitation = Boolean(existing.invite_account_id && previous.length);
  if (!next.inviteAccountId) return { bumpSequence: hadInvitation, recipients: [] };
  if (!hadInvitation || existing.invite_account_id !== next.inviteAccountId) return { bumpSequence: true, recipients: next.attendees };
  const time = (value: Date | string | null | undefined) => value == null ? Number.NaN : new Date(value).getTime();
  const text = (value: string | null | undefined) => String(value ?? '').trim();
  const detailsChanged = text(existing.summary) !== text(next.summary)
    || text(existing.description) !== text(next.description)
    || text(existing.location) !== text(next.location)
    || time(existing.starts_at) !== next.startsAt.getTime()
    || time(existing.ends_at) !== next.endsAt.getTime()
    || Boolean(existing.all_day) !== Boolean(next.allDay);
  if (detailsChanged) return { bumpSequence: true, recipients: next.attendees };
  const removed = previous.some(email => !next.attendees.includes(email));
  return { bumpSequence: removed, recipients: next.attendees.filter(email => !previous.includes(email)) };
}

function normalizeAttendees(value: unknown): string[] | null {
  if (!Array.isArray(value)) return null;
  const attendees = value.map(email => typeof email === 'string' ? email.trim().toLowerCase() : '').filter(Boolean);
  if (attendees.some(email => /[\r\n\0\s,;"<>]/.test(email) || !/^[^@]+@[^@]+\.[^@]+$/.test(email))) return null;
  return [...new Set(attendees)];
}

// `attendees` is a jsonb column. node-postgres serialises a JavaScript array as a
// PostgreSQL array literal (`{a@b.c}`), which jsonb rejects with
// "invalid input syntax for type json" — and an empty array silently becomes the
// jsonb OBJECT `{}`. Either way a plain array must never be bound directly.
function jsonbAttendees(value: unknown): string {
  return JSON.stringify(Array.isArray(value) ? value : []);
}

// Older rows may already hold `{}` because of the binding bug above. Read them
// back as an empty array and never call jsonb_array_length() on a non-array.
const ATTENDEES_IS_ARRAY = "jsonb_typeof(attendees) = 'array'";
// Reads must tolerate `{}` rows written before the binding fix; the CASE keeps
// jsonb_array_length() from ever seeing a non-array.
const READ_ATTENDEES = "CASE WHEN jsonb_typeof(attendees) = 'array' THEN attendees ELSE '[]'::jsonb END AS attendees";

function invitationOperationKey(req: Request): string {
  const supplied = req.headers['x-idempotency-key'];
  if (typeof supplied === 'string' && supplied.trim()) return supplied.trim().slice(0, 128);
  return crypto.createHash('sha256').update(JSON.stringify(req.body || {})).digest('hex');
}

function invitationRequestFingerprint(req: Request, fields: InvitationFields) {
  const { calendarId, normalizedAttendees, times, summary, description, location, url, organizer, allDay, timezone, invitationAccount } = fields;
  return crypto.createHash('sha256').update(JSON.stringify({
    eventId: req.params.eventId || null, calendarId, summary: summary || null, description, location, url, organizer,
    allDay: Boolean(allDay), timezone, attendees: normalizedAttendees, inviteAccountId: invitationAccount?.id || null,
    startsAt: times.startsAt.toISOString(), endsAt: times.endsAt.toISOString(),
  })).digest('hex');
}

// The single response shape for anything that may have to deliver an invitation.
function invitationDeliveryResponse(event: unknown, delivery: InvitationDeliveryStatus | null | undefined) {
  return {
    event,
    invitationStatus: { status: delivery?.status || 'pending', lastError: delivery?.lastError || null },
    ...(invitationDeliveryError(delivery) ? { invitationError: invitationDeliveryError(delivery) } : {}),
  };
}

async function updateInvitedEvent(req: Request, fields: InvitationFields) {
  const { calendarId, invitationAccount, normalizedAttendees, times, summary, description, location, url, organizer, allDay, timezone } = fields;
  const key = invitationOperationKey(req);
  const fingerprint = invitationRequestFingerprint(req, fields);
  return withTransaction(async client => {
    const prior = await client.query('SELECT id, event_id, request_fingerprint, status, last_error, payload FROM calendar_invitation_outbox WHERE user_id = $1 AND idempotency_key = $2 FOR UPDATE', [req.session.userId, key]);
    if (prior.rows[0]) {
      if (prior.rows[0].request_fingerprint !== fingerprint) return { conflict: true };
      const event = (await client.query('SELECT id, calendar_id, uid, etag, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invitation_sequence, created_at, updated_at FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3', [prior.rows[0].event_id, calendarId, req.session.userId])).rows[0];
      if (!event || event.id !== req.params.eventId) return { conflict: true };
      // An identical retry of an undelivered invitation must actually resend it,
      // not just replay the earlier error. The caller delivers after commit.
      if (prior.rows[0].status === 'sent') return { event, duplicate: true, delivered: { status: 'sent', lastError: null } };
      return { event, duplicate: true, outboxId: prior.rows[0].id, payload: prior.rows[0].payload };
    }
    const existing = (await client.query(`SELECT uid, raw_ical, ${READ_ATTENDEES}, invite_account_id, invitation_sequence, summary, description, location, starts_at, ends_at, all_day FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 FOR UPDATE`, [req.params.eventId, calendarId, req.session.userId])).rows[0];
    if (!existing) return { notFound: true };
    const hadInvitation = Boolean(existing.invite_account_id && Array.isArray(existing.attendees) && existing.attendees.length);
    const senderChanged = hadInvitation && invitationAccount.id !== existing.invite_account_id;
    const cancelledAttendees = hadInvitation
      ? (senderChanged ? existing.attendees : existing.attendees.filter((email: string) => !normalizedAttendees.includes(email)))
      : [];
    let cancellationAccount = null;
    if (cancelledAttendees.length) {
      cancellationAccount = invitationAccount.id === existing.invite_account_id
        ? invitationAccount
        : (await client.query<EmailAccountRow>('SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND smtp_host IS NOT NULL', [existing.invite_account_id, req.session.userId])).rows[0] || null;
      if (!cancellationAccount) return { cancelFailed: true };
    }
    const plan = invitationUpdatePlan(existing, { attendees: normalizedAttendees, inviteAccountId: invitationAccount.id, summary, description, location, allDay: Boolean(allDay), ...times });
    const rawIcal = mergeCalendarResource(existing.raw_ical, localEventIcal({ uid: existing.uid, summary, description, location, url, organizer, attendees: normalizedAttendees, partstats: preservedPartstats(existing, times), allDay: Boolean(allDay), ...times }));
    const result = await client.query(`UPDATE calendar_events SET raw_ical = $1, summary = $2, description = $3, location = $4, url = $5, organizer = $6, starts_at = $7, ends_at = $8, all_day = $9, timezone = $10, attendees = $11, invite_account_id = $12, invitation_sequence = CASE WHEN NOT $16::boolean THEN invitation_sequence WHEN (invite_account_id IS NOT NULL AND ${ATTENDEES_IS_ARRAY} AND jsonb_array_length(attendees) > 0) OR invitation_sequence > 0 THEN invitation_sequence + 1 ELSE 0 END, etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $13 AND calendar_id = $14 AND user_id = $15 RETURNING id, calendar_id, uid, etag, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invitation_sequence, created_at, updated_at`, [rawIcal, summary || null, description, location, url, organizer, times.startsAt, times.endsAt, Boolean(allDay), timezone, jsonbAttendees(normalizedAttendees), invitationAccount.id, req.params.eventId, calendarId, req.session.userId, plan.bumpSequence]);
    const event = result.rows[0];
    const actions = [];
    if (cancelledAttendees.length) actions.push({ account: cancellationAccount, attendees: cancelledAttendees, summary: existing.summary, description: existing.description, location: existing.location, uid: existing.uid, allDay: Boolean(existing.all_day), method: 'CANCEL', sequence: Number(existing.invitation_sequence || 0) + 1, startsAt: new Date(existing.starts_at).toISOString(), endsAt: new Date(existing.ends_at).toISOString() });
    if (plan.recipients.length) actions.push({ account: invitationAccount, attendees: plan.recipients, summary, description, location, uid: event.uid, allDay: Boolean(allDay), method: 'REQUEST', sequence: event.invitation_sequence, startsAt: times.startsAt.toISOString(), endsAt: times.endsAt.toISOString() });
    // Nothing for any attendee to hear about: the edit is saved without a mail.
    if (!actions.length) return { event, raw: rawIcal, uid: existing.uid, unchanged: true as const };
    const outbox = await client.query('INSERT INTO calendar_invitation_outbox (user_id, event_id, idempotency_key, request_fingerprint, payload) VALUES ($1,$2,$3,$4,$5::jsonb) RETURNING id', [req.session.userId, event.id, key, fingerprint, JSON.stringify({ actions: invitationActionsForStorage(actions) })]);
    return { event, outboxId: outbox.rows[0].id, actions, raw: rawIcal, uid: existing.uid };
  });
}

/** The calendar row fields a writable-calendar check and a remote push both need. */
type WritableCalendarRow = { id: string; user_id: string; source: string; read_only: boolean; external_url: string | null };

async function writableCalendar(userId: string, calendarId: string): Promise<{ status: number; error: string; calendar?: undefined } | { status?: undefined; error?: undefined; calendar: WritableCalendarRow }> {
  const result = await query<WritableCalendarRow>(
    'SELECT id, user_id, source, read_only, external_url FROM calendars WHERE id = $1 AND user_id = $2 AND owner_user_id = $2',
    [calendarId, userId],
  );
  const calendar = result.rows[0];
  if (!calendar) return { status: 404, error: 'Calendar not found' };
  // 'local' calendars are writable unless explicitly marked read-only. 'caldav'
  // calendars are writable only once the user has opted the source into two-way
  // sync (externalCalendarSync sets read_only = false on those rows when it does);
  // 'ical_url' calendars stay permanently read-only — they're static feeds.
  if (calendar.read_only || !['local', 'caldav'].includes(calendar.source)) return { status: 403, error: 'This calendar is read-only' };
  return { calendar };
}

// After a writable-calendar transaction commits, push the change to the remote
// CalDAV server if this calendar is one. A no-op for 'local' calendars and for
// 'caldav' calendars synced by ical_url-style direct URL only setup (pushEventToRemote
// checks calendar.source/read_only/the source's own `writable` flag itself).
async function pushIfExternal(calendar: WritableCalendarRow, uid: string, action: 'put' | 'delete', rawIcal?: string, remote?: { href: string | null; etag: string | null }) {
  if (calendar.source !== 'caldav') return null;
  const outcome = await pushEventToRemote({ calendar, uid, rawIcal, action, remote });
  if ('skipped' in outcome) return null;
  return outcome;
}

async function contactCalendarAppearance(userId: string): Promise<{ name?: string | null; color?: string | null; [key: string]: unknown }> {
  const result = await query<{ appearance?: { name?: string | null; color?: string | null; [key: string]: unknown } | null }>("SELECT preferences->'calendarContactAppearance' AS appearance FROM users WHERE id = $1", [userId]);
  return result?.rows?.[0]?.appearance || {};
}

// Fetch the raw .ics MIME part of a message. Extracted so the reader can fall back
// to it whenever the invitation captured during sync is missing or unusable.
async function fetchInvitationAttachment(row: InvitationMessageRow, userId: string) {
  const attachments: AttachmentRef[] = typeof row.attachments === 'string' ? JSON.parse(row.attachments) : row.attachments || [];
  const candidates = attachments.filter(item => /^(text\/calendar|application\/(ics|ical|calendar))$/i.test(item.type || '') || /\.ics$/i.test(item.filename || ''));
  if (candidates.length !== 1 || Number(candidates[0].size) > 1024 * 1024) return null;
  const account = await query<EmailAccountRow>('SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2', [row.account_id, userId]);
  if (!account.rows[0]) return null;
  const { imapManager } = await import('../index.js');
  let data;
  try {
    data = await imapManager.fetchAttachment(account.rows[0], row.uid, row.folder, candidates[0].part);
  } catch (caught) {
    const error = toAppError(caught);
    // The mailbox could not be reached right now. This is a fetch failure, not a
    // malformed invitation, and it must not turn into an opaque 500.
    console.warn('Calendar invitation attachment fetch failed:', error.message);
    return null;
  }
  if (!data) return null;
  // The fetched part is decoded bytes; the parser needs the iCalendar text.
  const raw = Buffer.isBuffer(data) ? data.toString('utf8') : String(data);
  return raw.trim() ? raw : null;
}

// An attendee's answer (METHOD:REPLY) to an invitation this user sent, read from the message.
async function readMessageReply(messageId: string, userId: string) {
  const result = await query<InvitationMessageRow>(`SELECT i.raw_ical, m.account_id, m.uid, m.folder, m.attachments
    FROM messages m JOIN email_accounts a ON a.id = m.account_id
    LEFT JOIN inbound_calendar_invitations i ON i.message_id = m.id
    WHERE m.id = $1 AND a.user_id = $2`, [messageId, userId]);
  const row = result?.rows?.[0];
  if (!row) return null;
  const raw = await fetchInvitationAttachment(row, userId);
  return raw ? parseInboundCalendarReply(raw) : null;
}

/** Whether the user holds the event (one they sent invitations from) a reply belongs to. */
async function replyIsRecorded(userId: string, uid: string) {
  const result = await query('SELECT 1 FROM calendar_events WHERE user_id = $1 AND uid = $2 AND invite_account_id IS NOT NULL LIMIT 1', [userId, uid]);
  return result.rows.length > 0;
}

async function readMessageInvitation(messageId: string, userId: string) {
  const result = await query<InvitationMessageRow>(`SELECT i.raw_ical, m.account_id, m.uid, m.folder, m.attachments
    FROM messages m JOIN email_accounts a ON a.id = m.account_id
    LEFT JOIN inbound_calendar_invitations i ON i.message_id = m.id
    WHERE m.id = $1 AND a.user_id = $2`, [messageId, userId]);
  const row = result.rows[0];
  if (!row) return null;
  // Prefer the invitation captured during sync. When it is absent, or present but
  // not parseable, fall back to the raw MIME part: a message that predates the
  // capture (or whose capture was incomplete) must still be openable and importable.
  const sources = [row.raw_ical, () => fetchInvitationAttachment(row, userId)];
  for (const source of sources) {
    const raw = typeof source === 'function' ? await source() : source;
    if (!raw) continue;
    const invitation = parseInboundCalendarInvitation(raw);
    if (invitation) return { ...invitation, event: parseCalendarEvent(raw), accountId: row.account_id };
  }
  return null;
}

// The local event a message's invitation was imported into, if any. The reader uses it
// to show an already-added invitation as added — and, on a cancellation, to offer
// removing the copy the organizer has just retracted. Scoped to the importing message
// so an unrelated local event with the same UID is never reported or removed.
async function importedEventForMessage(messageId: string, userId: string) {
  const result = await query(
    `SELECT id, calendar_id, invitation_sequence, starts_at, ends_at, all_day
     FROM calendar_events
     WHERE source_message_id = $1 AND user_id = $2
     ORDER BY created_at ASC LIMIT 1`,
    [messageId, userId],
  );
  const event = result.rows[0];
  return event ? {
    id: event.id, calendarId: event.calendar_id, sequence: Number(event.invitation_sequence || 0),
    startsAt: event.starts_at, endsAt: event.ends_at, allDay: event.all_day,
  } : null;
}

router.get('/invitations/:messageId', async (req, res) => {
  const invitation = await readMessageInvitation(req.params.messageId, sessionUserId(req));
  if (!invitation) {
    // An answer to an invitation this user sent: record it (idempotently — sync normally did
    // already) and describe it, so the mail shows who answered what.
    const reply = await readMessageReply(req.params.messageId, sessionUserId(req));
    if (!reply) return res.status(404).json({ error: 'Calendar invitation not found' });
    const updated = await applyInvitationReply(sessionUserId(req), reply);
    return res.json({ invitation: { ...reply, recordedInCalendar: updated > 0 || await replyIsRecorded(sessionUserId(req), reply.uid) } });
  }
  const { raw, event, accountId, ...metadata } = invitation;
  void raw; void accountId;
  const localEvent = await importedEventForMessage(req.params.messageId, sessionUserId(req));
  const response = await storedInvitationResponse(sessionUserId(req), invitation);
  res.json({ invitation: { ...metadata, localEvent, response, description: event?.description, location: event?.location, url: event?.url, attendees: event?.attendees || [] } });
});

const INVITATION_RESPONSES = new Set<InvitationResponse>(['ACCEPTED', 'TENTATIVE', 'DECLINED']);

type MessageInvitation = NonNullable<Awaited<ReturnType<typeof readMessageInvitation>>>;

const organizerAddress = (value: unknown) => String(value || '').replace(/^mailto:/i, '').trim();

/** The reply this user sent for this invitation, unless the organizer has revised it since. */
async function storedInvitationResponse(userId: string, invitation: MessageInvitation): Promise<InvitationResponse | null> {
  const result = await query<{ response: InvitationResponse }>(
    `SELECT response FROM calendar_invitation_responses
     WHERE user_id = $1 AND uid = $2 AND recurrence_id = $3 AND organizer = $4 AND sequence >= $5`,
    [userId, invitation.uid, invitation.recurrenceId || '', organizerAddress(invitation.organizer).toLowerCase(), Number(invitation.sequence || 0)],
  );
  return result.rows[0]?.response ?? null;
}

/**
 * The address this user was invited as: the receiving account's address or one of its aliases
 * that appears among the attendees, falling back to the account address.
 */
async function invitedAddress(userId: string, accountId: string, attendees: readonly string[]) {
  const result = await query<{ email: string | null; kind: string }>(
    `SELECT email_address AS email, 'account' AS kind FROM email_accounts WHERE id = $1 AND user_id = $2
     UNION ALL
     SELECT al.email, 'alias' AS kind FROM account_aliases al JOIN email_accounts a ON a.id = al.account_id
      WHERE al.account_id = $1 AND a.user_id = $2`,
    [accountId, userId],
  );
  const own = result.rows.map(row => String(row.email || '').trim()).filter(Boolean);
  const invited = new Set(attendees.map(address => address.trim().toLowerCase()));
  return own.find(address => invited.has(address.toLowerCase())) || own[0] || null;
}

/**
 * Add (or refresh) the local copy of a received invitation. With `partstat`, the user's own
 * ATTENDEE line records their answer so CalDAV clients show it too.
 */
async function importInvitationEvent(userId: string, messageId: string, calendar: WritableCalendarRow, invitation: MessageInvitation, partstat?: { email: string; response: InvitationResponse }) {
  const event = invitation.event;
  if (!event) throw Object.assign(new Error('This invitation cannot be added'), { status: 409 });
  // Scope copies by organizer as well as UID; never overwrite an unrelated local event.
  const uid = `mail-${crypto.createHash('sha256').update(JSON.stringify([invitation.uid, invitation.organizer, invitation.recurrenceId])).digest('hex')}`;
  const component = new ICAL.Component(ICAL.parse(event.raw));
  component.removeAllProperties('method');
  const vevent = component.getAllSubcomponents('vevent')[0];
  vevent.updatePropertyWithValue('uid', uid);
  if (partstat) {
    for (const attendee of vevent.getAllProperties('attendee')) {
      if (String(attendee.getFirstValue()).replace(/^mailto:/i, '').toLowerCase() === partstat.email.toLowerCase()) {
        attendee.setParameter('partstat', partstat.response);
      }
    }
  }
  const raw = component.toString();
  // A re-answer of the same revision must refresh the stored PARTSTAT, so `<=` here; an
  // older revision never overwrites a newer one, and an event the user owns is never touched.
  const result = await query(`INSERT INTO calendar_events
    (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, all_day, timezone, description, location, url, organizer, attendees, invitation_sequence, source_message_id)
    VALUES ($1,$2,$3,$4,gen_random_uuid()::text,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14::jsonb,$15,$16)
    ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE SET
      raw_ical = EXCLUDED.raw_ical, etag = gen_random_uuid()::text, summary = EXCLUDED.summary,
      starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone,
      description = EXCLUDED.description, location = EXCLUDED.location, url = EXCLUDED.url,
      organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees, invitation_sequence = EXCLUDED.invitation_sequence,
      source_message_id = COALESCE(EXCLUDED.source_message_id, calendar_events.source_message_id), updated_at = NOW()
    WHERE calendar_events.invitation_sequence ${partstat ? '<=' : '<'} EXCLUDED.invitation_sequence AND calendar_events.invite_account_id IS NULL
    RETURNING id`, [calendar.id, userId, uid, raw, event.summary, event.startsAt, event.endsAt,
      event.allDay, event.timeZone, event.description, event.location, event.url, event.organizer, JSON.stringify(event.attendees), invitation.sequence,
      messageId]);
  if (result.rows[0]) await pushIfExternal(calendar, uid, 'put', raw);
  return { changed: Boolean(result.rows[0]) };
}

// Remove the copy this message's invitation was added as. This is what makes a
// cancellation actionable: an organizer retracting an invitation should leave the
// calendar in the state it would have been in had the invitation never been accepted.
router.delete('/invitations/:messageId', async (req, res) => {
  const invitation = await readMessageInvitation(req.params.messageId, sessionUserId(req));
  if (!invitation) return res.status(404).json({ error: 'Calendar invitation not found' });
  const localEvent = await importedEventForMessage(req.params.messageId, sessionUserId(req));
  if (!localEvent) return res.status(404).json({ error: 'This invitation was not added to a calendar' });
  // A retraction that predates the copy we hold must not delete it: the organizer may
  // have sent a newer update since. Same ordering rule the import path already applies.
  if (Number(invitation.sequence || 0) < localEvent.sequence) {
    return res.status(409).json({ error: 'This cancellation is older than the event already in the calendar' });
  }
  // Only the imported mirror is removed, and only while it is still that mirror: an
  // event the user has since taken ownership of (by inviting attendees from it) is
  // left alone rather than silently deleted out from under them.
  const removed = await query(
    `DELETE FROM calendar_events
     WHERE id = $1 AND user_id = $2 AND source_message_id = $3 AND invite_account_id IS NULL
     RETURNING id`,
    [localEvent.id, req.session.userId, req.params.messageId],
  );
  if (!removed.rows[0]) return res.status(409).json({ error: 'This event can no longer be removed automatically' });
  res.json({ removed: true, calendarId: localEvent.calendarId });
});

router.post('/invitations/:messageId', async (req, res) => {
  if (!req.body?.calendarId) return res.status(400).json({ error: 'calendarId is required' });
  const access = await writableCalendar(sessionUserId(req), req.body.calendarId);
  if (access.error) return res.status(access.status).json({ error: access.error });
  const invitation = await readMessageInvitation(req.params.messageId, sessionUserId(req));
  if (!invitation) return res.status(404).json({ error: 'Calendar invitation not found' });
  if (invitation.method !== 'REQUEST' || !invitation.event) return res.status(409).json({ error: 'This invitation cannot be added' });
  const { changed } = await importInvitationEvent(sessionUserId(req), req.params.messageId, access.calendar as WritableCalendarRow, invitation);
  res.json({ added: true, changed, calendarId: req.body.calendarId });
});

// Answer a received invitation: send the RFC 5546 REPLY to the organizer, then keep the
// local calendar in step — accepted/tentative is added (or refreshed) with the user's PARTSTAT,
// declined removes the copy this message had added. The reply is sent first: if it fails,
// nothing is recorded and the calendar is unchanged, so the user can simply try again.
router.post('/invitations/:messageId/respond', async (req, res) => {
  const userId = sessionUserId(req);
  const response = String(req.body?.response || '').toUpperCase() as InvitationResponse;
  if (!INVITATION_RESPONSES.has(response)) return res.status(400).json({ error: 'response must be ACCEPTED, TENTATIVE or DECLINED' });
  const invitation = await readMessageInvitation(req.params.messageId, userId);
  if (!invitation) return res.status(404).json({ error: 'Calendar invitation not found' });
  if (invitation.method !== 'REQUEST' || !invitation.event) return res.status(409).json({ error: 'This invitation cannot be answered' });
  const organizer = organizerAddress(invitation.organizer);
  if (!organizer.includes('@')) return res.status(409).json({ error: 'The invitation has no organizer address to reply to' });

  let calendar: WritableCalendarRow | null = null;
  if (response !== 'DECLINED') {
    const existing = await importedEventForMessage(req.params.messageId, userId);
    const calendarId = req.body?.calendarId || existing?.calendarId;
    if (!calendarId) return res.status(400).json({ error: 'calendarId is required' });
    const access = await writableCalendar(userId, calendarId);
    if (access.error) return res.status(access.status).json({ error: access.error });
    calendar = access.calendar as WritableCalendarRow;
  }

  const attendee = await invitedAddress(userId, invitation.accountId, invitation.event.attendees || []);
  const accountResult = await query<EmailAccountRow>('SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2', [invitation.accountId, userId]);
  if (!attendee || !accountResult.rows[0]) return res.status(404).json({ error: 'The receiving account was not found' });
  try {
    await sendCalendarReply({
      account: accountResult.rows[0], attendeeEmail: attendee, organizerEmail: organizer, response,
      uid: invitation.uid, recurrenceId: invitation.recurrenceId || null, sequence: Number(invitation.sequence || 0),
      summary: invitation.summary, startsAt: invitation.event.startsAt, endsAt: invitation.event.endsAt, allDay: Boolean(invitation.event.allDay),
    });
  } catch (caught) {
    const err = toAppError(caught);
    console.error('Calendar invitation reply failed:', err.message);
    return res.status(502).json({ error: 'Could not send your reply to the organizer' });
  }

  await query(
    `INSERT INTO calendar_invitation_responses (user_id, uid, recurrence_id, organizer, sequence, response)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (user_id, uid, recurrence_id, organizer) DO UPDATE SET
       sequence = EXCLUDED.sequence, response = EXCLUDED.response, responded_at = NOW()`,
    [userId, invitation.uid, invitation.recurrenceId || '', organizer.toLowerCase(), Number(invitation.sequence || 0), response],
  );

  let localEvent: { calendarId: string } | null = null;
  if (calendar) {
    await importInvitationEvent(userId, req.params.messageId, calendar, invitation, { email: attendee, response });
    localEvent = { calendarId: calendar.id };
  } else {
    // Declining removes only the mirror this message added, never an event the user owns.
    await query(
      `DELETE FROM calendar_events WHERE user_id = $1 AND source_message_id = $2 AND invite_account_id IS NULL`,
      [userId, req.params.messageId],
    );
  }
  res.json({ response, localEvent });
});

// Who was invited to an event and how each answered (from its iCalendar ATTENDEE lines).
// An occurrence id ("series@recurrence") reads its series.
router.get('/events/:id/attendees', async (req, res) => {
  const id = String(req.params.id).split('@')[0];
  const result = await query<{ raw_ical: string }>(
    `SELECT e.raw_ical FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id
      WHERE e.id::text = $1 AND e.user_id = $2 AND c.owner_user_id = $2`,
    [id, sessionUserId(req)],
  );
  if (!result.rows[0]) return res.status(404).json({ error: 'Event not found' });
  res.json({ attendees: attendeeStatuses(result.rows[0].raw_ical) });
});

router.get('/calendars', async (req, res) => {
  const result = await query(
    `SELECT id, name, description, color, source, external_url, read_only, display_visible, owner_user_id, sync_token, created_at, updated_at
     FROM calendars WHERE user_id = $1 AND owner_user_id = $1 ORDER BY created_at ASC`,
    [req.session.userId],
  );
  const appearance = await contactCalendarAppearance(sessionUserId(req));
  res.json({ calendars: [...result.rows, {
    id: 'contacts-birthdays', name: appearance.name || 'Contact dates', custom_name: Boolean(appearance.name), description: 'Birthdays and anniversaries from contacts',
    color: appearance.color || '#e879f9', source: 'contacts', external_url: null, read_only: true, display_visible: appearance.displayVisible !== false,
  }] });
});

function calendarName(value: unknown): string | null {
  const name = typeof value === 'string' ? value.trim() : '';
  return name && name.length <= 120 ? name : null;
}

function calendarColor(value: unknown): string | null | undefined {
  if (value == null || value === '') return null;
  return typeof value === 'string' && /^#[0-9a-f]{6}$/i.test(value) ? value : undefined;
}

router.post('/calendars', async (req, res) => {
  const name = calendarName(req.body?.name);
  const color = calendarColor(req.body?.color);
  const displayVisible = req.body?.displayVisible ?? true;
  if (!name || color === undefined || typeof displayVisible !== 'boolean') {
    return res.status(400).json({ error: 'name, a hex color, and displayVisible are required' });
  }
  try {
    const result = await query(
      `INSERT INTO calendars (user_id, owner_user_id, name, color, display_visible, source, read_only)
       VALUES ($1, $1, $2, $3, $4, 'local', false)
       RETURNING id, user_id, owner_user_id, name, description, color, source, external_url, read_only, display_visible, sync_token, created_at, updated_at`,
      [req.session.userId, name, color, displayVisible],
    );
    return res.status(201).json({ calendar: result.rows[0] });
  } catch (caught) {
    const error = toAppError(caught);
    if (error.code === '23505') return res.status(409).json({ error: 'A calendar with that name already exists' });
    throw error;
  }
});

router.patch('/calendars/:calendarId', async (req, res) => {
  const name = calendarName(req.body?.name);
  const color = calendarColor(req.body?.color);
  const displayVisible = req.body?.displayVisible;
  if (!name || color === undefined || typeof displayVisible !== 'boolean') {
    return res.status(400).json({ error: 'name, a hex color, and displayVisible are required' });
  }
  if (req.params.calendarId === 'contacts-birthdays') {
    const customName = req.body.customName === false ? null : name;
    await query(
      "UPDATE users SET preferences = COALESCE(preferences, '{}'::jsonb) || jsonb_build_object('calendarContactAppearance', $2::jsonb) WHERE id = $1",
      [req.session.userId, JSON.stringify({ name: customName, color, displayVisible })],
    );
    return res.json({ calendar: { id: 'contacts-birthdays', name: customName || 'Contact dates', custom_name: Boolean(customName), color, display_visible: displayVisible, source: 'contacts', read_only: true } });
  }
  try {
    const result = await query(
      `UPDATE calendars
       SET name = $1, color = $2, display_visible = $3, updated_at = NOW()
       WHERE id = $4 AND owner_user_id = $5 AND user_id = $5
       RETURNING id, user_id, owner_user_id, name, description, color, source, external_url, read_only, display_visible, sync_token, created_at, updated_at`,
      [name, color, displayVisible, req.params.calendarId, req.session.userId],
    );
    if (!result.rows[0]) return res.status(404).json({ error: 'Calendar not found' });
    return res.json({ calendar: result.rows[0] });
  } catch (caught) {
    const error = toAppError(caught);
    if (error.code === '23505') return res.status(409).json({ error: 'A calendar with that name already exists' });
    throw error;
  }
});

router.delete('/calendars/:calendarId', async (req, res) => {
  const confirmName = calendarName(req.body?.confirmName);
  if (!confirmName) return res.status(400).json({ error: 'confirmName is required' });
  const result = await query(
    `DELETE FROM calendars
     WHERE id = $1 AND owner_user_id = $2 AND user_id = $2 AND name = $3 AND source = 'local' AND read_only = false
     RETURNING id`,
    [req.params.calendarId, req.session.userId, confirmName],
  );
  if (!result.rows[0]) return res.status(404).json({ error: 'Calendar not found' });
  res.status(204).end();
});

router.get('/events', async (req, res) => {
  const from = new Date(queryString(req.query.from) ?? '');
  const to = new Date(queryString(req.query.to) ?? '');
  if (Number.isNaN(from.getTime()) || Number.isNaN(to.getTime()) || to < from) {
    return res.status(400).json({ error: 'A valid from/to range is required' });
  }
  if (to.getTime() - from.getTime() > MAX_EVENT_RANGE_MS) {
    return res.status(400).json({ error: 'The requested event range is too large' });
  }
  const selection = parseCalendarSelection(req.query.calendarIds);
  if (selection.error) return res.status(400).json({ error: selection.error });
  const selectedIds = selection.ids === null ? null : selection.ids.filter(id => id !== CONTACT_CALENDAR_ID);
  const includeContacts = selection.ids === null || selection.ids.includes(CONTACT_CALENDAR_ID);
  // A selection naming only the contact calendar still needs no event query, and
  // an explicitly empty selection means "no calendars at all". Ownership stays
  // enforced by the SQL predicate, so a foreign id can only match zero rows.
  let materializedRows: CalendarEventRow[] = [];
  let eventRows: CalendarEventRow[] = [];
  if (selectedIds === null || selectedIds.length > 0) {
    const params: unknown[] = [req.session.userId, from, to];
    let calendarFilter = '';
    if (selectedIds !== null) { params.push(selectedIds); calendarFilter = ' AND c.id = ANY($4::uuid[])'; }
    // Materialised occurrences: a plain indexed range scan, with no recurrence expansion at
    // all. Any event whose stored rows are missing, stale, or do not cover this window is
    // excluded here and picked up by the fallback query below, so this read can never be the
    // reason an event is missing — only the reason it appears fast.
    const result = await query<CalendarEventRow>(
      `SELECT CASE WHEN o.recurrence_id = '' THEN e.id::text ELSE e.id::text || '@' || o.recurrence_id END AS id,
              CASE WHEN e.recurring THEN e.id END AS series_id,
              e.recurring,
              o.recurrence_id, o.starts_at, o.ends_at, o.all_day, o.timezone,
              COALESCE(o.summary, e.summary) AS summary,
              COALESCE(o.description, e.description) AS description,
              COALESCE(o.location, e.location) AS location,
              COALESCE(o.url, e.url) AS url,
              COALESCE(o.organizer, e.organizer) AS organizer,
              COALESCE(o.attendees, e.attendees) AS attendees,
              e.calendar_id, e.uid, e.etag, e.invite_account_id, e.invitation_sequence,
              CASE WHEN sa.id IS NOT NULL THEN e.source_message_id END AS source_message_id,
              sm.folder AS source_folder,
              sa.id AS source_account_id,
              c.name AS calendar_name, c.color AS calendar_color, c.source, c.read_only
       FROM calendar_occurrences o
       JOIN calendar_events e ON e.id = o.event_id
       JOIN calendars c ON c.id = o.calendar_id
       LEFT JOIN messages sm ON sm.id = e.source_message_id
       LEFT JOIN email_accounts sa ON sa.id = sm.account_id AND sa.user_id = e.user_id
       LEFT JOIN calendar_occurrence_state s ON s.event_id = e.id
       WHERE o.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1
         AND o.starts_at < $3 AND o.ends_at > $2
         AND NOT ${coveragePredicate('s')}${calendarFilter}
       ORDER BY o.starts_at ASC`,
      params,
    );
    materializedRows = result.rows;

    // The live fallback. Everything not covered above is expanded exactly as it was before
    // materialisation existed, which is what makes a lagging or broken worker a performance
    // problem rather than a correctness one.
    // `e.recurring` is a stored, indexed column rather than a regex over raw_ical: the regex
    // could not use an index, so the planner scanned every event the user owned and detoasted
    // each raw_ical. See migration 0082.
    const fallback = await query<CalendarEventRow>(
      `SELECT ${EVENT_COLUMNS},
              CASE WHEN sa.id IS NOT NULL THEN e.source_message_id END AS source_message_id,
              sm.folder AS source_folder,
              sa.id AS source_account_id,
              c.name AS calendar_name, c.color AS calendar_color, c.source, c.read_only
       FROM calendar_events e
       JOIN calendars c ON c.id = e.calendar_id
       LEFT JOIN messages sm ON sm.id = e.source_message_id
       LEFT JOIN email_accounts sa ON sa.id = sm.account_id AND sa.user_id = e.user_id
       LEFT JOIN calendar_occurrence_state s ON s.event_id = e.id
       WHERE e.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1
         AND ((e.starts_at < $3 AND e.ends_at > $2) OR e.recurring)
         AND ${coveragePredicate('s')}${calendarFilter}
       ORDER BY e.starts_at ASC`,
      params,
    );
    eventRows = fallback.rows;
  }
  let contactEvents: Array<{ starts_at: Date; ends_at: Date; [key: string]: unknown }> = [];
  if (includeContacts) {
    // Both reads only need the same user id, so they run together rather than one after
    // the other — the contact calendar is on by default, so this is on the common path.
    const [contactResult, appearance] = await Promise.all([
      query(
        'SELECT id, display_name, primary_email, birthday, anniversary, contact_dates FROM contacts WHERE user_id = $1 AND (birthday IS NOT NULL OR anniversary IS NOT NULL OR (jsonb_typeof(contact_dates) = \'array\' AND jsonb_array_length(contact_dates) > 0))',
        [req.session.userId],
      ),
      contactCalendarAppearance(sessionUserId(req)),
    ]);
    contactEvents = contactDateEvents(contactResult?.rows || [], from, to).map(event => ({
      ...event, calendar_name: appearance.name || event.calendar_name, calendar_custom_name: Boolean(appearance.name), calendar_color: appearance.color || event.calendar_color,
    }));
  }
  // Only the events the fast read could not serve are expanded here — normally none. The
  // worker pool still bounds the CPU when it does happen, so a single request cannot stall
  // the process on a series the worker has not reached yet.
  const projection = await projectCalendarResources(eventRows, from, to, { userId: req.session.userId });
  const events = [...materializedRows, ...projection.events, ...contactEvents]
    .sort((left, right) => eventStartTime(left.starts_at) - eventStartTime(right.starts_at));
  if (projection.truncated) {
    // A partial result must never look complete. Only the series id and a reason
    // category cross the wire; internal error text stays in the server log.
    for (const failure of projection.failures) {
      console.warn('Calendar projection incomplete:', JSON.stringify({ userId: req.session.userId, seriesId: failure.id, reason: failure.reason }));
    }
    return res.json({
      events,
      truncated: true,
      incompleteSeries: projection.failures.map(failure => ({ series_id: failure.id, reason: failure.reason || 'truncated' })),
    });
  }
  res.json({ events, truncated: false });
});

// Simple in-memory rate limiter: 20 searches per minute per user — same bucket shape and
// limit as the mail/contacts search routes (search.ts), kept local here rather than shared
// since that limiter is module-private and this is a different, independent query shape.
const calendarSearchBuckets = new Map<string, { count: number; resetAt: number }>();
setInterval(() => {
  const now = Date.now();
  for (const [key, bucket] of calendarSearchBuckets) {
    if (now > bucket.resetAt) calendarSearchBuckets.delete(key);
  }
}, 60_000);

function calendarSearchLimiter(req: Request, res: Response, next: NextFunction) {
  const key = sessionUserId(req);
  const now = Date.now();
  const bucket = calendarSearchBuckets.get(key);
  if (!bucket || now > bucket.resetAt) {
    calendarSearchBuckets.set(key, { count: 1, resetAt: now + 60_000 });
    return next();
  }
  if (bucket.count >= 20) {
    res.setHeader('Retry-After', Math.ceil((bucket.resetAt - now) / 1000));
    return res.status(429).json({ error: 'Too many search requests. Try again shortly.' });
  }
  bucket.count += 1;
  next();
}

// How far "no horizon" actually reaches. A real Date/ICAL walk still needs concrete bounds,
// so this is not literally unbounded — it is wide enough that no realistic calendar event or
// recurrence falls outside it either way, which is the only sense in which a search feature
// can honestly promise "everything". The per-series iteration cap inside projectCalendarResources
// (not this window) is what actually bounds the work for a pathological open-ended RRULE.
const SEARCH_HORIZON_YEARS = 100;
function unboundedSearchWindow(now = new Date()) {
  return {
    from: new Date(now.getFullYear() - SEARCH_HORIZON_YEARS, 0, 1),
    to: new Date(now.getFullYear() + SEARCH_HORIZON_YEARS, 0, 1),
  };
}

// Full-text search across every calendar event the user owns — not scoped to the currently
// viewed month/week/day range the way GET /events is, and NOT limited to the occurrence-
// materialisation horizon either (CALENDAR_OCCURRENCE_HORIZON_*_MONTHS; see occurrenceHorizon).
// That window exists purely to make the normal month/week/day *render* path fast — a search is
// a deliberate, infrequent, rate-limited action, so it can afford to match against the base
// calendar_events rows directly (always present, regardless of what's materialised) and then
// live-project only what matched over `unboundedSearchWindow`, via the same recurrence engine
// and per-series iteration cap the normal view's live-fallback path already relies on for an
// uncovered event. A non-recurring matched row projects to exactly itself; a recurring one
// expands to every one of its individual occurrences within the window (each tagged with its
// own series_id/recurrence_id, same addressing GET /events uses) — not just its one base
// definition. Contact-calendar birthdays/anniversaries are synthetic (computed per request, not
// stored text), so they are not included.
router.get('/events/search', calendarSearchLimiter, async (req, res) => {
  const q = (queryString(req.query.q) ?? '').trim();
  if (!q) return res.json({ events: [] });
  if (q.length > 200) return res.status(400).json({ error: 'Search query is too long' });
  // `|| 200` would also catch a literal `limit=0` (falsy) and silently replace it with the
  // default instead of clamping it up to the minimum of 1 — Number.isFinite tells "absent or
  // not a number" apart from "a real, if degenerate, value" so only the former falls back.
  const parsedLimit = Number.parseInt(queryString(req.query.limit) ?? '', 10);
  const limit = Math.min(Math.max(Number.isFinite(parsedLimit) ? parsedLimit : 200, 1), 500);
  const like = `%${q}%`;
  const matched = await query<CalendarEventRow>(
    `SELECT ${EVENT_COLUMNS},
            CASE WHEN sa.id IS NOT NULL THEN e.source_message_id END AS source_message_id,
            sm.folder AS source_folder,
            sa.id AS source_account_id,
            c.name AS calendar_name, c.color AS calendar_color, c.source, c.read_only
     FROM calendar_events e
     JOIN calendars c ON c.id = e.calendar_id
     LEFT JOIN messages sm ON sm.id = e.source_message_id
     LEFT JOIN email_accounts sa ON sa.id = sm.account_id AND sa.user_id = e.user_id
     WHERE e.user_id = $1 AND c.user_id = $1 AND c.owner_user_id = $1
       AND (e.summary ILIKE $2 OR e.description ILIKE $2 OR e.location ILIKE $2)`,
    [req.session.userId, like],
  );
  if (!matched.rows.length) return res.json({ events: [] });
  const { from, to } = unboundedSearchWindow();
  const projection = await projectCalendarResources(matched.rows, from, to, { userId: req.session.userId });
  const events = projection.events
    .sort((left, right) => eventStartTime(right.starts_at) - eventStartTime(left.starts_at))
    .slice(0, limit);
  res.json({ events, truncated: projection.truncated });
});

router.post('/events', async (req, res) => {
  const { calendarId, summary, description: rawDescription = null, location = null, url = null, organizer = null, allDay = false, timezone = null, sendInvites = false, inviteAccountId, attendees } = req.body || {};
  const description = normalizeDescription(rawDescription);
  const times = parseEventTimes(req.body);
  if (!calendarId || !times) return res.status(400).json({ error: 'calendarId and a valid event range are required' });
  const normalizedAttendees = normalizeAttendees(attendees || []);
  if (!normalizedAttendees) return res.status(400).json({ error: 'Attendees must be valid email addresses' });
  if (sendInvites && (!inviteAccountId || !normalizedAttendees.length)) {
    return res.status(400).json({ error: 'A sender account and at least one attendee are required for invitations' });
  }

  const access = await writableCalendar(sessionUserId(req), calendarId);
  if (access.error) return res.status(access.status).json({ error: access.error });

  let invitationAccount = null;
  if (sendInvites) {
    const sender = await query<EmailAccountRow>(
      'SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND enabled = true AND smtp_host IS NOT NULL',
      [inviteAccountId, req.session.userId],
    );
    invitationAccount = sender.rows[0] || null;
    if (!invitationAccount) return res.status(400).json({ error: 'The selected sender account is unavailable' });
  }

  if (sendInvites && invitationAccount && typeof req.headers['x-idempotency-key'] === 'string' && req.headers['x-idempotency-key'].trim()) {
    const idempotencyKey = invitationOperationKey(req);
    const fingerprint = invitationRequestFingerprint(req, { calendarId, normalizedAttendees, times, summary, description, location, url, organizer, allDay, timezone, invitationAccount });
    let outcome;
    try {
      outcome = await withTransaction(async client => {
      const prior = await client.query('SELECT id, event_id, request_fingerprint, status, last_error, payload FROM calendar_invitation_outbox WHERE user_id = $1 AND idempotency_key = $2 FOR UPDATE', [req.session.userId, idempotencyKey]);
      if (prior.rows[0]) {
        if (prior.rows[0].request_fingerprint !== fingerprint) return { conflict: true };
        const event = (await client.query('SELECT id, calendar_id, uid, etag, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invitation_sequence, created_at, updated_at FROM calendar_events WHERE id = (SELECT event_id FROM calendar_invitation_outbox WHERE id = $1)', [prior.rows[0].id])).rows[0];
        if (prior.rows[0].status === 'sent') return { event, duplicate: true, delivered: { status: 'sent', lastError: null } };
        return { event, duplicate: true, outboxId: prior.rows[0].id, payload: prior.rows[0].payload };
      }
      const uid = crypto.randomUUID();
      const rawIcal = localEventIcal({ uid, summary, description, location, url, organizer, attendees: normalizedAttendees, allDay: Boolean(allDay), ...times });
      const result = await client.query(
        `INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
         RETURNING id, calendar_id, uid, etag, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invitation_sequence, created_at, updated_at`,
        [calendarId, req.session.userId, uid, rawIcal, summary || null, description, location, url, organizer, times.startsAt, times.endsAt, Boolean(allDay), timezone, jsonbAttendees(normalizedAttendees), invitationAccount.id],
      );
      const event = result.rows[0];
      const outbox = await client.query(
        `INSERT INTO calendar_invitation_outbox (user_id, event_id, idempotency_key, request_fingerprint, payload) VALUES ($1,$2,$3,$4,$5::jsonb) RETURNING id`,
        [req.session.userId, event.id, idempotencyKey, fingerprint, JSON.stringify({ actions: invitationActionsForStorage([{ account: invitationAccount, attendees: normalizedAttendees, summary, description, location, uid, allDay: Boolean(allDay), method: 'REQUEST', sequence: event.invitation_sequence ?? 0, startsAt: times.startsAt.toISOString(), endsAt: times.endsAt.toISOString() }]) })],
      );
        return { event, outboxId: outbox.rows[0].id, raw: rawIcal };
      });
    } catch (caught) {
      const error = toAppError(caught);
      console.error('Calendar invitation transaction failed:', error.message, error.code ? `(code ${error.code})` : '');
      return res.status(500).json({ error: 'The event and invitation could not be saved; no partial changes were kept.' });
    }
    if (outcome.conflict) return res.status(409).json({ error: 'The idempotency key was already used for a different calendar operation' });
    if (outcome.duplicate) {
      if (outcome.delivered) return res.status(201).json(invitationDeliveryResponse(outcome.event, outcome.delivered));
      // Same request, same key, invitation not delivered yet: resend it now rather
      // than replaying the stale error. The account is resolved from the payload.
      const delivered = await deliverStoredInvitation({ userId: req.session.userId, outboxId: outcome.outboxId, payload: outcome.payload, fallbackAccountId: outcome.event?.invite_account_id });
      return res.status(201).json(invitationDeliveryResponse(outcome.event, delivered));
    }
    // Mirror the new event to a CalDAV calendar before anything else: an event that never
    // reached the server is removed again by the next sync of that calendar.
    const remoteSync = await pushIfExternal(access.calendar as WritableCalendarRow, outcome.event.uid, 'put', outcome.raw);
    const actions = await resolveInvitationActions(sessionUserId(req), [{ account: invitationAccount, attendees: normalizedAttendees, summary, description, location, uid: outcome.event.uid, allDay: Boolean(allDay), method: 'REQUEST', sequence: outcome.event.invitation_sequence ?? 0, startsAt: times.startsAt.toISOString(), endsAt: times.endsAt.toISOString() }]);
    const delivered = await deliverInvitationOutbox({ outboxId: outcome.outboxId, actions });
    return res.status(201).json({ ...invitationDeliveryResponse(outcome.event, delivered), ...(remoteSync ? { remoteSync } : {}) });
  }

  const uid = crypto.randomUUID();
  const rawIcal = localEventIcal({ uid, summary, description, location, url, organizer, attendees: normalizedAttendees, allDay: Boolean(allDay), ...times });
  const result = await query<{ id: string; calendar_id: string; uid: string; etag?: string | null; summary?: string | null; description?: string | null; location?: string | null; url?: string | null; organizer?: string | null; starts_at?: string | Date | null; ends_at?: string | Date | null; all_day?: boolean | null; timezone?: string | null; attendees?: unknown; invite_account_id?: string | null; invitation_sequence?: number | null; created_at?: string | Date | null }>(
    `INSERT INTO calendar_events (
       calendar_id, user_id, uid, raw_ical, summary, description, location, url, organizer,
       starts_at, ends_at, all_day, timezone, attendees, invite_account_id
     ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
     RETURNING id, calendar_id, uid, etag, summary, description, location, url, organizer,
               starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invitation_sequence, created_at, updated_at`,
    [calendarId, req.session.userId, uid, rawIcal, summary || null, description, location, url, organizer, times.startsAt, times.endsAt, Boolean(allDay), timezone, jsonbAttendees(normalizedAttendees), invitationAccount?.id || null],
  );
  let invitationError = null;
  if (invitationAccount) {
    try {
      await sendCalendarInvitation({ account: invitationAccount, attendees: normalizedAttendees, summary, description, location, uid, allDay: Boolean(allDay), method: 'REQUEST', sequence: result.rows[0].invitation_sequence ?? 0, ...times });
    } catch (caught) {
      const error = toAppError(caught);
      invitationError = 'The event was saved, but the invitation could not be sent.';
      console.error('Calendar invitation delivery failed:', error.message);
    }
  }
  const remoteSync = await pushIfExternal(access.calendar as WritableCalendarRow, uid, 'put', rawIcal);
  res.status(201).json({ event: result.rows[0], ...(invitationError ? { invitationError } : {}), ...(remoteSync ? { remoteSync } : {}) });
});


router.all('/events/:eventId/occurrence', async (req, res) => {
  if (!['PATCH', 'DELETE'].includes(req.method)) return res.status(405).end();
  const { calendarId, recurrenceId } = req.body || {};
  if (!calendarId || typeof recurrenceId !== 'string' || !/^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}:\d{2}Z?)?$/.test(recurrenceId)) return res.status(400).json({ error: 'A valid occurrence and calendar are required' });
  // 'single' changes only the occurrence named; 'following' ends the series just before it.
  // Deleting the whole series is the plain event DELETE, which already handles invitations.
  const scope = req.body?.scope === 'following' ? 'following' : 'single';
  if (scope !== 'single' && req.method !== 'DELETE') return res.status(400).json({ error: 'Only a cancellation can affect following occurrences' });
  const access = await writableCalendar(sessionUserId(req), calendarId);
  if (access.error) return res.status(access.status).json({ error: access.error });
  const cancel = req.method === 'DELETE';
  const times = cancel ? null : parseEventTimes(req.body);
  const attendees = normalizeAttendees(req.body.attendees || []);
  if (!cancel && (!times || !attendees)) return res.status(400).json({ error: 'Invalid event values' });
  const outcome = await withTransaction(async client => {
    const row = (await client.query('SELECT uid, raw_ical, invite_account_id, remote_href, remote_etag FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 FOR UPDATE', [req.params.eventId, calendarId, req.session.userId])).rows[0];
    if (!row) return { status: 404 };
    if (scope === 'following') {
      const truncated = truncateSeriesBefore(row.raw_ical, recurrenceId);
      if (!truncated) return { status: 409 };
      if (truncated.empty) {
        // Cancelling from the series' own first occurrence leaves nothing, so remove the event
        // rather than keep a series that produces no occurrences. Captured before the row is
        // gone — pushIfExternal can no longer look this up itself once this commits.
        await client.query('DELETE FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3', [req.params.eventId, calendarId, req.session.userId]);
        return { status: 200, uid: row.uid, deleted: true, remoteHref: row.remote_href ? decrypt(row.remote_href) : null, remoteEtag: row.remote_etag || null };
      }
      await client.query('UPDATE calendar_events SET raw_ical = $1, etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $2 AND calendar_id = $3 AND user_id = $4', [truncated.raw, req.params.eventId, calendarId, req.session.userId]);
      return { status: 200, uid: row.uid, raw: truncated.raw };
    }
    const event = parseCalendarEvent(row.raw_ical);
    if (!event) return { status: 409 };
    const replacement = localEventIcal(cancel ? { ...event, allDay: event.allDay } : { ...req.body, description: normalizeDescription(req.body?.description), attendees, ...times, uid: row.uid });
    const raw = mergeCalendarResource(row.raw_ical, replacement, recurrenceId, cancel);
    await client.query('UPDATE calendar_events SET raw_ical = $1, etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $2 AND calendar_id = $3 AND user_id = $4', [raw, req.params.eventId, calendarId, req.session.userId]);
    return { status: 200, uid: row.uid, raw };
  });
  if (outcome.status !== 200) return res.status(outcome.status).json({ error: 'Calendar occurrence unavailable' });
  const remoteSync = outcome.deleted
    ? await pushIfExternal(access.calendar as WritableCalendarRow, outcome.uid, 'delete', undefined, { href: outcome.remoteHref, etag: outcome.remoteEtag })
    : await pushIfExternal(access.calendar as WritableCalendarRow, outcome.uid, 'put', outcome.raw);
  res.json({ updated: true, scope, ...(remoteSync ? { remoteSync } : {}) });
});

router.patch('/events/:eventId', async (req, res) => {
  const { calendarId, summary, description: rawDescription = null, location = null, url = null, organizer = null, allDay = false, timezone = null, sendInvites = false, inviteAccountId, attendees } = req.body || {};
  const description = normalizeDescription(rawDescription);
  const times = parseEventTimes(req.body);
  if (!calendarId || !times) return res.status(400).json({ error: 'calendarId and a valid event range are required' });
  const normalizedAttendees = normalizeAttendees(attendees || []);
  if (!normalizedAttendees) return res.status(400).json({ error: 'Attendees must be valid email addresses' });
  if (sendInvites && (!inviteAccountId || !normalizedAttendees.length)) return res.status(400).json({ error: 'A sender account and at least one attendee are required for invitations' });

  const access = await writableCalendar(sessionUserId(req), calendarId);
  if (access.error) return res.status(access.status).json({ error: access.error });

  let invitationAccount = null;
  if (sendInvites) {
    const sender = await query<EmailAccountRow>('SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND enabled = true AND smtp_host IS NOT NULL', [inviteAccountId, req.session.userId]);
    invitationAccount = sender.rows[0] || null;
    if (!invitationAccount) return res.status(400).json({ error: 'The selected sender account is unavailable' });
  }

  if (sendInvites && invitationAccount && typeof req.headers['x-idempotency-key'] === 'string' && req.headers['x-idempotency-key'].trim()) {
    let outcome;
    try {
      outcome = await updateInvitedEvent(req, { calendarId, invitationAccount, normalizedAttendees, times, summary, description, location, url, organizer, allDay, timezone });
    } catch (caught) {
      const error = toAppError(caught);
      console.error('Calendar invitation transaction failed:', error.message, error.code ? `(code ${error.code})` : '');
      return res.status(500).json({ error: 'The event and invitation could not be saved; no partial changes were kept.' });
    }
    if (outcome.notFound) return res.status(404).json({ error: 'Event not found' });
    if (outcome.conflict) return res.status(409).json({ error: 'The idempotency key was already used for a different calendar update' });
    if (outcome.cancelFailed) return res.status(502).json({ error: 'The previous invitation could not be cancelled, so the event was not changed.' });
    if (outcome.duplicate) {
      if (outcome.delivered) return res.json(invitationDeliveryResponse(outcome.event, outcome.delivered));
      // An identical retry must resend an undelivered invitation, not replay the error.
      const delivered = await deliverStoredInvitation({ userId: req.session.userId, outboxId: outcome.outboxId, payload: outcome.payload, fallbackAccountId: outcome.event?.invite_account_id });
      return res.json(invitationDeliveryResponse(outcome.event, delivered));
    }
    // The edit is committed: mirror it to a CalDAV calendar, or the next sync would take
    // the remote copy (without this edit — or, for a new event, without the event at all).
    const remoteSync = await pushIfExternal(access.calendar as WritableCalendarRow, outcome.uid, 'put', outcome.raw);
    if (outcome.unchanged) return res.json({ ...invitationDeliveryResponse(outcome.event, { status: 'sent', lastError: null }), ...(remoteSync ? { remoteSync } : {}) });
    const actions = await resolveInvitationActions(sessionUserId(req), outcome.actions);
    const delivered = await deliverInvitationOutbox({ outboxId: outcome.outboxId, actions });
    return res.json({ ...invitationDeliveryResponse(outcome.event, delivered), ...(remoteSync ? { remoteSync } : {}) });
  }

  const outcome = await withTransaction(async client => {
    const existing = await client.query(`SELECT uid, raw_ical, ${READ_ATTENDEES}, invite_account_id, invitation_sequence, summary, description, location, starts_at, ends_at, all_day FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 FOR UPDATE`, [req.params.eventId, calendarId, req.session.userId]);
    const existingEvent = existing.rows[0];
    if (!existingEvent) return { notFound: true };

    const hadInvitation = Boolean(existingEvent.invite_account_id && Array.isArray(existingEvent.attendees) && existingEvent.attendees.length);
    const senderChanged = hadInvitation && sendInvites && invitationAccount?.id !== existingEvent.invite_account_id;
    const cancelledAttendees = hadInvitation ? (senderChanged || !sendInvites ? existingEvent.attendees : existingEvent.attendees.filter((email: string) => !normalizedAttendees.includes(email))) : [];
    const cancellationAccount = invitationAccount?.id === existingEvent.invite_account_id
      ? invitationAccount
      : cancelledAttendees.length
        // A disabled account retains SMTP settings for cancellation; referenced
        // sender accounts cannot be deleted because the FK is ON DELETE RESTRICT.
        ? (await client.query<EmailAccountRow>('SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND smtp_host IS NOT NULL', [existingEvent.invite_account_id, req.session.userId])).rows[0] || null
        : null;
    if (cancelledAttendees.length) {
      if (!cancellationAccount) return { cancelFailed: true };
      try {
        await sendCalendarInvitation({ account: cancellationAccount, attendees: cancelledAttendees, summary: existingEvent.summary, description: existingEvent.description, location: existingEvent.location, uid: existingEvent.uid, allDay: Boolean(existingEvent.all_day), method: 'CANCEL', sequence: Number(existingEvent.invitation_sequence || 0) + 1, startsAt: new Date(existingEvent.starts_at), endsAt: new Date(existingEvent.ends_at) });
      } catch (caught) {
        const error = toAppError(caught);
        console.error('Calendar invitation cancellation before update failed:', error.message);
        return { cancelFailed: true };
      }
    }

    const plan = invitationUpdatePlan(existingEvent, { attendees: normalizedAttendees, inviteAccountId: invitationAccount?.id || null, summary, description, location, allDay: Boolean(allDay), ...times });
    const rawIcal = mergeCalendarResource(existingEvent.raw_ical, localEventIcal({ uid: existingEvent.uid, summary, description, location, url, organizer, attendees: normalizedAttendees, partstats: preservedPartstats(existingEvent, times), allDay: Boolean(allDay), ...times }));
    const result = await client.query(`UPDATE calendar_events SET raw_ical = $1, summary = $2, description = $3, location = $4, url = $5, organizer = $6, starts_at = $7, ends_at = $8, all_day = $9, timezone = $10, attendees = $11, invite_account_id = $12, invitation_sequence = CASE WHEN NOT $16::boolean THEN invitation_sequence WHEN (invite_account_id IS NOT NULL AND ${ATTENDEES_IS_ARRAY} AND jsonb_array_length(attendees) > 0) OR invitation_sequence > 0 THEN invitation_sequence + 1 ELSE 0 END, etag = gen_random_uuid()::text, updated_at = NOW() WHERE id = $13 AND calendar_id = $14 AND user_id = $15 RETURNING id, calendar_id, uid, etag, summary, description, location, url, organizer, starts_at, ends_at, all_day, timezone, attendees, invite_account_id, invitation_sequence, created_at, updated_at`, [rawIcal, summary || null, description, location, url, organizer, times.startsAt, times.endsAt, Boolean(allDay), timezone, jsonbAttendees(normalizedAttendees), invitationAccount?.id || null, req.params.eventId, calendarId, req.session.userId, plan.bumpSequence]);
    if (!result.rows[0]) return { notFound: true };

    let delivered = null;
    if (invitationAccount && plan.recipients.length) {
      try {
        // Keep the row lock until this REQUEST is emitted, so a later mutation
        // cannot overtake it with a higher sequence number.
        await sendCalendarInvitation({ account: invitationAccount, attendees: plan.recipients, summary, description, location, uid: existingEvent.uid, allDay: Boolean(allDay), method: 'REQUEST', sequence: result.rows[0].invitation_sequence, ...times });
        delivered = { status: 'sent', lastError: null };
      } catch (caught) {
        const error = toAppError(caught);
        delivered = { status: 'failed', lastError: error.message };
        console.error('Calendar invitation delivery failed:', error.message, error.code ? `(code ${error.code})` : '');
      }
    }
    return { event: result.rows[0], delivered, raw: rawIcal, uid: existingEvent.uid };
  });
  if (outcome.cancelFailed) return res.status(502).json({ error: 'The previous invitation could not be cancelled, so the event was not changed.' });
  if (outcome.notFound || !outcome.event) return res.status(404).json({ error: 'Event not found' });

  const remoteSync = await pushIfExternal(access.calendar as WritableCalendarRow, outcome.uid, 'put', outcome.raw);
  res.json({ ...invitationDeliveryResponse(outcome.event, outcome.delivered || { status: 'sent', lastError: null }), ...(remoteSync ? { remoteSync } : {}) });
});


router.delete('/events/:eventId', async (req, res) => {
  const calendarId = typeof req.query.calendarId === 'string' ? req.query.calendarId : null;
  if (!calendarId) return res.status(400).json({ error: 'calendarId is required' });

  const access = await writableCalendar(sessionUserId(req), calendarId);
  if (access.error) return res.status(access.status).json({ error: access.error });

  const outcome = await withTransaction(async client => {
    const existing = await client.query(`SELECT uid, raw_ical, ${READ_ATTENDEES}, invite_account_id, invitation_sequence, summary, description, location, starts_at, ends_at, all_day, remote_href, remote_etag FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 FOR UPDATE`, [req.params.eventId, calendarId, req.session.userId]);
    const event = existing.rows[0];
    if (!event) return { notFound: true };

    if (event.invite_account_id && Array.isArray(event.attendees) && event.attendees.length) {
      try {
        // A disabled account retains SMTP settings for cancellation; referenced
        // sender accounts cannot be deleted because the FK is ON DELETE RESTRICT.
        const sender = await client.query<EmailAccountRow>('SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2 AND smtp_host IS NOT NULL', [event.invite_account_id, req.session.userId]);
        if (!sender.rows[0]) return { cancelFailed: true };
        await sendCalendarInvitation({ account: sender.rows[0], attendees: event.attendees, summary: event.summary, description: event.description, location: event.location, uid: event.uid, allDay: Boolean(event.all_day), method: 'CANCEL', sequence: Number(event.invitation_sequence || 0) + 1, startsAt: new Date(event.starts_at), endsAt: new Date(event.ends_at) });
      } catch (caught) {
        const error = toAppError(caught);
        console.error('Calendar invitation cancellation before deletion failed:', error.message);
        return { cancelFailed: true };
      }
    }

    const result = await client.query('DELETE FROM calendar_events WHERE id = $1 AND calendar_id = $2 AND user_id = $3 RETURNING id', [req.params.eventId, calendarId, req.session.userId]);
    // Captured before the row is gone — pushIfExternal can no longer look this up
    // itself once the local delete below has committed.
    return { deleted: Boolean(result.rows[0]), uid: event.uid, remoteHref: event.remote_href ? decrypt(event.remote_href) : null, remoteEtag: event.remote_etag || null };
  });
  if (outcome.cancelFailed) return res.status(502).json({ error: 'The invitation could not be cancelled, so the event was not deleted.' });
  if (outcome.notFound || !outcome.deleted) return res.status(404).json({ error: 'Event not found' });

  await pushIfExternal(access.calendar as WritableCalendarRow, outcome.uid, 'delete', undefined, { href: outcome.remoteHref, etag: outcome.remoteEtag });
  res.status(204).end();
});

/** A calendar_import_sources row as the source routes return, redact and schedule it. */
type CalendarSourceRow = {
  id: string;
  kind: string;
  url: string;
  user_id?: string;
  url_fingerprint?: string | null;
  username?: string;
  password?: string;
  display_name?: string | null;
  color?: string | null;
  interval_seconds?: number;
  writable?: boolean;
  collections?: string | null;
  enabled?: boolean | null;
  last_sync_at?: string | Date | null;
  last_error?: string | null;
  [key: string]: unknown;
};

function publicSource(source: CalendarSourceRow) {
  const decryptedUrl = source.url ? decrypt(source.url) : null;
  const secretValues = decryptedUrl === null
    ? (source.url ? [source.url] : [])
    : [source.url, decryptedUrl];
  const lastError = typeof source.last_error === 'string'
    ? secretValues.filter(Boolean).reduce((error, secret) => error.replaceAll(secret, '[redacted]'), source.last_error)
    : source.last_error;
  const collections = parseSourceCollections(source);
  return {
    id: source.id, kind: source.kind,
    displayName: source.display_name, color: source.color, intervalSeconds: source.interval_seconds,
    writable: Boolean(source.writable), enabled: source.enabled, lastSyncAt: source.last_sync_at, lastError,
    // Collection hrefs never leave the server — only an opaque id (the href hash)
    // the client sends back to select/deselect it, same secrecy as `url`.
    collections: collections ? collections.map(c => ({ id: c.hrefHash, displayName: c.displayName, color: c.color, selected: c.selected })) : null,
  };
}

router.get('/sources', async (req, res) => {
  const result = await query<CalendarSourceRow>(
    `SELECT id, kind, url, username, display_name, color, interval_seconds, writable, collections, enabled, last_sync_at, last_error
     FROM calendar_import_sources WHERE user_id = $1 ORDER BY created_at ASC`, [req.session.userId],
  );
  res.json({ sources: result.rows.map(publicSource) });
});

// CalDAV discovery + the unified multi-protocol server manager now live at
// POST /api/dav/servers/discover (routes/davServers.ts). This route stays
// kind-agnostic for ical_url subscriptions and direct source edits.
router.post('/sources', async (req, res) => {
  const { kind, url, username, password, displayName, color = null, intervalSeconds, writable = false, collections: incoming } = req.body || {};
  if (!['caldav', 'ical_url'].includes(kind) || !displayName) return res.status(400).json({ error: 'kind and displayName are required' });
  if (kind === 'ical_url' && !url) return res.status(400).json({ error: 'url is required' });
  if (kind === 'caldav' && (!username || !password)) return res.status(400).json({ error: 'CalDAV sources require username and password' });
  // A CalDAV url is optional — when left blank, username must be the account's email
  // address, and its domain resolves the server the same way a well-known lookup does.
  const resolvedUrl = kind === 'caldav' ? resolveCaldavUrl(url, username) : { url: String(url) };
  if ('error' in resolvedUrl) return res.status(400).json({ error: resolvedUrl.error });
  const validated = await validateSourceUrl(resolvedUrl.url);
  if ('error' in validated) return res.status(400).json({ error: validated.error });
  const { url: parsed, policy } = validated;

  // Discovered collections (kind='caldav' only — ical_url stays a single feed).
  // Every href is attacker-influenced client JSON, so it gets the same SSRF check
  // as the top-level source URL before it's ever stored or fetched.
  let collectionsPayload: string | null = null;
  if (kind === 'caldav' && Array.isArray(incoming) && incoming.length) {
    const built = [];
    for (const entry of incoming as IncomingCollection[]) {
      const validatedHref = await validatedCollectionHref(entry?.href, policy);
      if ('error' in validatedHref) return res.status(400).json({ error: validatedHref.error });
      built.push({ ...validatedHref, displayName: entry.displayName || 'Calendar', color: entry.color || null, selected: entry.selected !== false });
    }
    collectionsPayload = encrypt(JSON.stringify(built));
  }

  const interval = clampIntervalSeconds(intervalSeconds);
  try {
    const normalizedUrl = parsed.toString();
    const urlFingerprint = crypto.createHash('sha256').update(normalizedUrl).digest('hex');
    const result = await query<CalendarSourceRow>(
      `INSERT INTO calendar_import_sources (user_id, kind, url, url_fingerprint, username, password, display_name, color, interval_seconds, writable, collections)
       VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *`,
      [req.session.userId, kind, encrypt(normalizedUrl), urlFingerprint, username || null, password ? encrypt(password) : null, displayName, color, interval, kind === 'caldav' && Boolean(writable), collectionsPayload],
    );
    const source = result.rows[0];
    scheduleCalendarSource(source);
    const sync = await syncCalendarSource(sessionUserId(req), source.id);
    if (!sync.ok) {
      // The sync records the failure asynchronously from the insert result;
      // reflect that terminal state in the response so the client can render
      // the persisted source as retryable immediately.
      source.last_error = sync.error;
      return res.status(502).json({ error: sync.error, source: publicSource(source), sync });
    }
    res.status(201).json({ source: publicSource(source), sync });
  } catch (caught) {
    const error = toAppError(caught);
    if (error.code === '23505') return res.status(409).json({ error: 'A source with this URL already exists' });
    if (error.code === '23514') return res.status(409).json({ error: 'Calendar source URL could not be stored securely' });
    throw error;
  }
});

router.post('/sources/:sourceId/sync', async (req, res) => {
  const result = await syncCalendarSource(sessionUserId(req), req.params.sourceId);
  if (!result.ok && result.error === 'Calendar source not found') return res.status(404).json({ error: result.error });
  res.json(result);
});

// Edit an existing source. Every field is optional and independently editable —
// name, credentials, cadence, the writable opt-in, and (for a CalDAV account with
// more than one calendar) which of its discovered calendars are imported.
router.patch('/sources/:sourceId', async (req, res) => {
  const existingResult = await query<CalendarSourceRow>('SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2', [req.params.sourceId, req.session.userId]);
  const existing = existingResult.rows[0];
  if (!existing) return res.status(404).json({ error: 'Calendar source not found' });

  const body = req.body || {};
  const sets: string[] = [];
  const values: unknown[] = [];
  const push = (column: string, value: unknown) => { values.push(value); sets.push(`${column} = $${values.length}`); };

  if (body.displayName !== undefined) {
    if (typeof body.displayName !== 'string' || !body.displayName.trim()) return res.status(400).json({ error: 'displayName must not be empty' });
    push('display_name', body.displayName.trim());
  }
  if (body.color !== undefined) push('color', body.color || null);
  if (body.intervalSeconds !== undefined) {
    const interval = Number.parseInt(body.intervalSeconds, 10);
    if (!Number.isInteger(interval) || interval < 15 || interval > 86400) {
      return res.status(400).json({ error: 'intervalSeconds must be between 15 and 86400' });
    }
    push('interval_seconds', interval);
  }
  if (body.enabled !== undefined) push('enabled', Boolean(body.enabled));
  if (body.username !== undefined) push('username', body.username || null);
  if (body.password) push('password', encrypt(body.password)); // blank/omitted keeps the stored password
  let writableChanged = false;
  if (body.writable !== undefined && existing.kind === 'caldav') {
    push('writable', Boolean(body.writable));
    writableChanged = true;
  }

  let nextCollections: SourceCollection[] | null = null;
  if (existing.kind === 'caldav' && Array.isArray(body.collections)) {
    const incoming = body.collections as IncomingCollection[];
    const policy = await getConnectionPolicy();
    if (incoming.some(entry => entry?.href)) {
      // A full replace, from a re-run of discovery in the edit dialog.
      const built = [];
      for (const entry of incoming) {
        const validatedHref = await validatedCollectionHref(entry?.href, policy);
        if ('error' in validatedHref) return res.status(400).json({ error: validatedHref.error });
        built.push({ ...validatedHref, displayName: entry.displayName || 'Calendar', color: entry.color || null, selected: entry.selected !== false });
      }
      nextCollections = built;
    } else {
      // A selection-only toggle against the already-discovered list.
      const current = parseSourceCollections(existing) || [];
      const toggles = new Map(incoming.filter(entry => entry?.id).map(entry => [entry.id, Boolean(entry.selected)]));
      nextCollections = current.map(collection => toggles.has(collection.hrefHash) ? { ...collection, selected: toggles.get(collection.hrefHash)! } : collection);
    }
    push('collections', encrypt(JSON.stringify(nextCollections)));
  }

  if (!sets.length) return res.status(400).json({ error: 'No editable fields were provided' });
  values.push(req.params.sourceId, req.session.userId);
  const result = await query<CalendarSourceRow>(
    `UPDATE calendar_import_sources SET ${sets.join(', ')}, updated_at = NOW()
     WHERE id = $${values.length - 1} AND user_id = $${values.length} RETURNING *`,
    values,
  );
  const source = result.rows[0];
  if (!source) return res.status(404).json({ error: 'Calendar source not found' });

  // Re-arm the timer — the scheduler closes over the source row it was given, so
  // without this a new interval or a pause/resume would not take effect until restart.
  if (source.enabled) scheduleCalendarSource(source);
  else { await stopCalendarSource(source.id); releaseCalendarSource(source.id); }

  // A calendar's writability lives on its own `calendars` row (read_only); flip every
  // row derived from this source to match, so already-imported calendars pick up the
  // change immediately instead of waiting for the next sync to (not) touch it.
  if (writableChanged) {
    await query(
      `UPDATE calendars SET read_only = $1, updated_at = NOW()
       WHERE user_id = $2 AND owner_user_id = $2 AND (external_url = $3 OR external_url LIKE $3 || ':%')`,
      [!source.writable, req.session.userId, `source:${source.id}`],
    );
  }
  // A calendar deselected from a multi-calendar source is removed locally — there's
  // no dedicated "remove one imported calendar" route, and leaving an orphaned,
  // never-synced-again calendar around would be a confusing dead end.
  if (nextCollections) {
    const deselected = nextCollections.filter(collection => !collection.selected).map(collection => `source:${source.id}:${collection.hrefHash}`);
    if (deselected.length) await query('DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND external_url = ANY($2::text[])', [req.session.userId, deselected]);
  }
  res.json({ source: publicSource(source) });
});

router.delete('/sources/:sourceId', async (req, res) => {
  const existing = await query('SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2', [req.params.sourceId, req.session.userId]);
  if (!existing.rows[0]) return res.status(404).json({ error: 'Calendar source not found' });
  let sourceDeleted = false;
  try {
    await stopCalendarSource(req.params.sourceId);
    const result = await query('DELETE FROM calendar_import_sources WHERE id = $1 AND user_id = $2 RETURNING id', [req.params.sourceId, req.session.userId]);
    if (!result.rows[0]) {
      releaseCalendarSource(req.params.sourceId);
      return res.status(404).json({ error: 'Calendar source not found' });
    }
    sourceDeleted = true;
    // A multi-calendar source's derived calendars carry a hash suffix (source:<id>:<hash>);
    // a single-calendar (or pre-discovery) source keeps the plain source:<id> key.
    await query(
      `DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND (external_url = $2 OR external_url LIKE $2 || ':%')`,
      [req.session.userId, `source:${req.params.sourceId}`],
    );
    releaseCalendarSource(req.params.sourceId);
    res.status(204).end();
  } catch (error) {
    releaseCalendarSource(req.params.sourceId);
    if (!sourceDeleted) scheduleCalendarSource(existing.rows[0]);
    throw error;
  }
});

export default router;
