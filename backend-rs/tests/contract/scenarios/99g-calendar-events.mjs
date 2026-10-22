// /api/calendar event mutations (with invitations mailed through the SMTP sink and the outbox),
// occurrence edits of a recurring series, and received invitations: read, add, answer, remove.
import { createCipheriv, randomBytes } from 'node:crypto';
import { A, M, U, seedMail } from '../seed-mail.mjs';
import { sink } from '../smtp-sink.mjs';

const SINK = { node: sink(2557), rust: sink(2558) };
const KEY = Buffer.from('1'.repeat(64), 'hex');
const CAL = '00000000-0000-4000-8000-0000000ca101';
const FEED = '00000000-0000-4000-8000-0000000ca102';
const SERIES = '00000000-0000-4000-8000-0000000e0101';

function encrypt(text) {
  const iv = randomBytes(12);
  const c = createCipheriv('aes-256-gcm', KEY, iv);
  const enc = Buffer.concat([c.update(text, 'utf8'), c.final()]);
  return `enc:v1:${iv.toString('hex')}:${c.getAuthTag().toString('hex')}:${enc.toString('hex')}`;
}

const REQUEST = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Ext//EN', 'METHOD:REQUEST', 'BEGIN:VEVENT', 'UID:ext-meeting-1', 'SEQUENCE:2',
  'DTSTAMP:20261001T080000Z', 'DTSTART:20261105T100000Z', 'DTEND:20261105T110000Z', 'SUMMARY:Planning, Q4',
  'ORGANIZER;CN=Org:mailto:org@ext.test', 'ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:alice@work.test',
  'ATTENDEE;PARTSTAT=ACCEPTED:mailto:other@ext.test', 'LOCATION:Room 1', 'DESCRIPTION:Agenda', 'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');
const WEEKLY = [
  'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Inboxora//DAV Hub//EN', 'BEGIN:VEVENT', 'UID:series-1', 'DTSTAMP:20260101T000000Z',
  'DTSTART:20261102T090000Z', 'DTEND:20261102T093000Z', 'RRULE:FREQ=WEEKLY;COUNT=6', 'SUMMARY:Weekly', 'END:VEVENT', 'END:VCALENDAR', '',
].join('\r\n');

async function seed(ctx) {
  const out = await seedMail(ctx);
  const { sql, backend } = ctx;
  for (const key of ['allow_private_hosts', 'allow_insecure_tls', 'allow_nonstandard_ports']) {
    await sql(`INSERT INTO system_settings (key, value) VALUES ($1, 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'`, [key]);
  }
  await sql(`UPDATE email_accounts SET smtp_host = '127.0.0.1', smtp_port = $2, smtp_tls = 'none', smtp_auth_user = 'alice', smtp_auth_pass = $3, sender_name = 'Alice W' WHERE id = $1`, [A.aliceWork, SINK[backend].port, encrypt('pw')]);
  await sql(`DELETE FROM calendars WHERE user_id IN ($1, $2)`, [U.alice, U.bob]);
  await sql(`INSERT INTO calendars (id, user_id, owner_user_id, name, source, created_at) VALUES ($1, $3, $3, 'Mine', 'local', '2026-01-01'), ($2, $3, $3, 'Feed', 'ical_url', '2026-01-02')`, [CAL, FEED, U.alice]);
  await sql(`INSERT INTO calendar_events (id, calendar_id, user_id, uid, raw_ical, summary, starts_at, ends_at, all_day, etag) VALUES ($1, $2, $3, 'series-1', $4, 'Weekly', '2026-11-02T09:00:00Z', '2026-11-02T09:30:00Z', false, 'e1')`, [SERIES, CAL, U.alice, WEEKLY]);
  await sql(`INSERT INTO inbound_calendar_invitations (message_id, method, state, uid, recurrence_id, sequence, summary, organizer, starts_at, ends_at, all_day, timezone, raw_ical)
    VALUES ($1, 'REQUEST', 'pending', 'ext-meeting-1', '', 2, 'Planning, Q4', 'mailto:org@ext.test', '2026-11-05T10:00:00Z', '2026-11-05T11:00:00Z', false, NULL, $2)`, [M(1), REQUEST]);
  await sql(`DELETE FROM calendar_invitation_outbox WHERE user_id = $1`, [U.alice]);
  await sql(`DELETE FROM calendar_invitation_responses WHERE user_id = $1`, [U.alice]);
  SINK[backend].messages.length = 0;
  return { ...out, keep: [...out.keep, CAL, FEED, SERIES], sink: backend };
}

// Every MIME part's body decoded by its own Content-Transfer-Encoding, joined.
function decoded(raw) {
  return raw.split(/\r\n--[^\r\n]*\r\n/).map((part) => {
    const at = part.indexOf('\r\n\r\n');
    if (at < 0) return part;
    const head = part.slice(0, at).replace(/\r\n[ \t]+/g, ' ');
    const body = part.slice(at + 4);
    const cte = (head.match(/^Content-Transfer-Encoding:\s*(\S+)/mi)?.[1] || '').toLowerCase();
    if (cte === 'base64') return Buffer.from(body.replace(/\s+/g, ''), 'base64').toString('utf8');
    if (cte === 'quoted-printable') {
      const bytes = body.replace(/=\r\n/g, '').replace(/=([0-9A-F]{2})/gi, (_, h) => String.fromCharCode(parseInt(h, 16)));
      return Buffer.from(bytes, 'latin1').toString('utf8');
    }
    return body;
  }).join('\n');
}

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const req = (method, path, body, extra = {}) => ({ method, path: `/api/calendar${path}`, as: 'alice', ...(body ? { body } : {}), ...extra });
const base = { calendarId: CAL, startsAt: '2026-11-10T10:00:00Z', endsAt: '2026-11-10T11:00:00Z' };
const invite = { ...base, summary: 'Review', location: 'HQ', sendInvites: true, inviteAccountId: A.aliceWork, attendees: ['Bob@X.test', 'carol@corp.test'] };

export default [{
  name: 'calendar events: create, update, delete, occurrences and invitations',
  seed,
  sessions,
  ignoreKeys: ['created_at', 'updated_at', 'etag'],
  steps: [
    req('POST', '/events', { calendarId: CAL }),
    req('POST', '/events', { ...base, endsAt: base.startsAt }),
    req('POST', '/events', { ...base, attendees: ['not an address'] }),
    req('POST', '/events', { ...base, sendInvites: true, attendees: ['a@b.co'] }),
    req('POST', '/events', { ...base, calendarId: FEED }),
    req('POST', '/events', { ...base, calendarId: '00000000-0000-4000-8000-0000000ca1ff' }),
    req('POST', '/events', { ...base, sendInvites: true, inviteAccountId: A.bob, attendees: ['a@b.co'] }),
    { ...req('POST', '/events', { ...base, summary: 'Plain', description: 'Line 1\nLine 2', allDay: false }), capture: (j) => ({ plain: j.event.id }) },
    { ...req('POST', '/events', invite, { headers: { 'X-Idempotency-Key': 'inv-1' } }), capture: (j) => ({ invited: j.event.id }) },
    req('POST', '/events', invite, { headers: { 'X-Idempotency-Key': 'inv-1' } }),
    req('POST', '/events', { ...invite, summary: 'Other' }, { headers: { 'X-Idempotency-Key': 'inv-1' } }),
    (c) => req('PATCH', `/events/${c.invited}`, { ...invite, summary: 'Review v2', attendees: ['bob@x.test', 'dan@corp.test'] }, { headers: { 'X-Idempotency-Key': 'inv-2' } }),
    (c) => req('PATCH', `/events/${c.invited}`, { ...invite, summary: 'Review v2', attendees: ['bob@x.test', 'dan@corp.test'] }, { headers: { 'X-Idempotency-Key': 'inv-3' } }),
    (c) => req('PATCH', `/events/${c.plain}`, { ...base, summary: 'Plain 2' }),
    (c) => req('PATCH', `/events/${c.plain}`, { ...base, calendarId: FEED }),
    req('PATCH', '/events/00000000-0000-4000-8000-0000000e01ff', base),
    req('PATCH', `/events/${SERIES}/occurrence`, { calendarId: CAL, recurrenceId: '2026-11-09T09:00:00Z', startsAt: '2026-11-09T12:00:00Z', endsAt: '2026-11-09T12:30:00Z', summary: 'Moved' }),
    req('PATCH', `/events/${SERIES}/occurrence`, { calendarId: CAL, recurrenceId: 'bad' }),
    req('PATCH', `/events/${SERIES}/occurrence`, { calendarId: CAL, recurrenceId: '2026-11-09T09:00:00Z', scope: 'following' }),
    req('DELETE', `/events/${SERIES}/occurrence`, { calendarId: CAL, recurrenceId: '2026-11-16T09:00:00Z' }),
    req('DELETE', `/events/${SERIES}/occurrence`, { calendarId: CAL, recurrenceId: '2026-11-23T09:00:00Z', scope: 'following' }),
    req('PUT', `/events/${SERIES}/occurrence`, { calendarId: CAL }),
    // Event reads after these writes depend on when each backend's occurrence materialiser
    // ticks; 70-calendar covers reads.
    (c) => req('GET', `/events/${c.invited}/attendees`),
    req('DELETE', '/events/x'),
    (c) => req('DELETE', `/events/${c.invited}?calendarId=${CAL}`),
    (c) => req('DELETE', `/events/${c.invited}?calendarId=${CAL}`),
    req('GET', `/invitations/${M(1)}`),
    req('GET', `/invitations/${M(3)}`),
    req('POST', `/invitations/${M(1)}`, {}),
    req('POST', `/invitations/${M(1)}`, { calendarId: FEED }),
    req('DELETE', `/invitations/${M(1)}`),
    req('POST', `/invitations/${M(1)}`, { calendarId: CAL }),
    req('POST', `/invitations/${M(1)}`, { calendarId: CAL }),
    req('POST', `/invitations/${M(1)}/respond`, { response: 'maybe' }),
    req('POST', `/invitations/${M(1)}/respond`, { response: 'accepted' }),
    req('GET', `/invitations/${M(1)}`),
    req('POST', `/invitations/${M(1)}/respond`, { response: 'DECLINED' }),
    req('DELETE', `/invitations/${M(1)}`),
  ],
  after: async ({ sql, ctx }) => {
    const mails = SINK[ctx.sink].messages.map((m) => {
      const head = m.data.split('\r\n\r\n')[0].replace(/\r\n[ \t]+/g, ' ');
      const ics = (decoded(m.data).match(/BEGIN:VCALENDAR[\s\S]*?END:VCALENDAR/) || [''])[0]
        .replace(/\r\n[ \t]/g, '').split(/\r\n/).filter(l => !/^(DTSTAMP|UID):/.test(l));
      return { to: [...m.to].sort(), subject: head.match(/^Subject: (.*)$/mi)?.[1] ?? null, ics };
    });
    return {
      mails,
      events: (await sql(`SELECT summary, starts_at, all_day, attendees, invitation_sequence, description, source_message_id FROM calendar_events WHERE user_id = $1 ORDER BY summary`, [U.alice])).rows,
      outbox: (await sql(`SELECT idempotency_key, status, attempts, last_error FROM calendar_invitation_outbox WHERE user_id = $1 ORDER BY idempotency_key`, [U.alice])).rows,
      responses: (await sql(`SELECT uid, recurrence_id, organizer, sequence, response FROM calendar_invitation_responses WHERE user_id = $1`, [U.alice])).rows,
    };
  },
}];
