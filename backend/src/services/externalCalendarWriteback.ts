// Pushes a local edit/delete on a writable CalDAV calendar back to the remote
// server. Only called for calendars with source='caldav' and read_only=false
// (see writableCalendar() in routes/calendar.ts) — every other calendar is a
// no-op here. Uses last-write-wins with refetch-on-conflict: an ETag mismatch
// never silently overwrites the remote copy; instead the local row is updated
// to match what the server actually has, and the caller is told to say so.
import crypto from 'crypto';
import { query } from './db.js';
import { decrypt, encrypt } from './encryption.js';
import { getConnectionPolicy } from './connectionPolicy.js';
import { parseCalendarEvent } from '../utils/ical.js';
import { parseSourceCollections } from './externalCalendarSync.js';
import { putCalendarEvent, deleteCalendarEvent } from './caldavClient.js';
import { toAppError } from '../utils/errors.js';

export type WriteBackOutcome =
  | { skipped: true }
  | { ok: true }
  | { ok: false; conflict: true; message: string }
  | { ok: false; conflict: false; error: string };

/** The calendar row fields a push needs — the same shape `writableCalendar()` already selects. */
export interface WritableCalendarRow { id: string; source: string; read_only: boolean; external_url: string | null; user_id: string }

/** The calendar_import_sources columns a push needs to read. */
interface CalendarImportSourceRow { id: string; kind: string; url: string; username: string | null; password: string | null; writable: boolean; collections: string | null; [key: string]: unknown }

function parseExternalUrl(externalUrl: string): { sourceId: string; hrefHashValue: string | null } | null {
  const match = externalUrl.match(/^source:([^:]+)(?::(.+))?$/);
  if (!match) return null;
  return { sourceId: match[1], hrefHashValue: match[2] || null };
}

async function applyRemoteConflict(calendarId: string, userId: string, uid: string, href: string, remote: { ical: string | null; etag: string | null }) {
  if (!remote.ical) {
    await query('DELETE FROM calendar_events WHERE calendar_id = $1 AND uid = $2 AND recurrence_id = $3', [calendarId, uid, '']);
    return;
  }
  const event = parseCalendarEvent(remote.ical);
  if (!event) return;
  const localEtag = crypto.createHash('sha256').update(event.raw).digest('hex');
  await query(
    `INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, all_day, timezone, description, location, url, organizer, attendees, remote_href, remote_etag)
     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::jsonb,$16,$17)
     ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE SET raw_ical = EXCLUDED.raw_ical,
       etag = EXCLUDED.etag, summary = EXCLUDED.summary, starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at,
       all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone, description = EXCLUDED.description,
       location = EXCLUDED.location, url = EXCLUDED.url, organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees,
       remote_href = EXCLUDED.remote_href, remote_etag = EXCLUDED.remote_etag, updated_at = NOW()`,
    [calendarId, userId, event.uid, event.raw, localEtag, event.summary, event.startsAt, event.endsAt, event.allDay, event.timeZone, event.description, event.location, event.url, event.organizer, JSON.stringify(event.attendees), encrypt(href), remote.etag],
  );
}

export async function pushEventToRemote({ calendar, uid, rawIcal, action, remote }: { calendar: WritableCalendarRow; uid: string; rawIcal?: string; action: 'put' | 'delete'; remote?: { href: string | null; etag: string | null } }): Promise<WriteBackOutcome> {
  if (calendar.source !== 'caldav' || calendar.read_only || !calendar.external_url) return { skipped: true };
  const parsed = parseExternalUrl(calendar.external_url);
  if (!parsed) return { skipped: true };

  const sourceResult = await query<CalendarImportSourceRow>('SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2', [parsed.sourceId, calendar.user_id]);
  const source = sourceResult.rows[0];
  if (!source || source.kind !== 'caldav' || !source.writable) return { skipped: true };

  const collectionHref = parsed.hrefHashValue
    ? (parseSourceCollections(source) || []).find(collection => collection.hrefHash === parsed.hrefHashValue)?.href || null
    : decrypt(source.url ?? '');
  if (!collectionHref) return { ok: false, conflict: false, error: 'The remote calendar location is unavailable' };

  const password = decrypt(source.password ?? '');
  if (!source.username || !password) return { ok: false, conflict: false, error: 'Stored calendar source credentials are unavailable' };
  const policy = await getConnectionPolicy();
  const creds = { username: source.username, password, allowPrivate: policy.allowPrivateHosts };

  // `remote` is supplied by callers that delete the local row before this runs — by
  // then it's too late to look the resource identity up here, since the row (and the
  // remote_href/remote_etag it carried) is already gone. Callers that only update the
  // local row (it still exists afterwards) can leave this to the lookup below.
  let remoteHref: string | null;
  let remoteEtag: string | null;
  if (remote) {
    remoteHref = remote.href; remoteEtag = remote.etag;
  } else {
    const eventRow = (await query<{ remote_href: string | null; remote_etag: string | null }>(
      'SELECT remote_href, remote_etag FROM calendar_events WHERE calendar_id = $1 AND uid = $2 AND recurrence_id = $3',
      [calendar.id, uid, ''],
    )).rows[0];
    remoteHref = eventRow?.remote_href ? decrypt(eventRow.remote_href) : null;
    remoteEtag = eventRow?.remote_etag || null;
  }

  try {
    if (action === 'delete') {
      if (!remoteHref) return { ok: true }; // never pushed to the remote yet — nothing to delete there
      const result = await deleteCalendarEvent({ href: remoteHref, ifMatchEtag: remoteEtag, ...creds });
      if (result.ok) return { ok: true };
      if (result.conflict) {
        await applyRemoteConflict(calendar.id, calendar.user_id, uid, remoteHref, result.remote);
        return { ok: false, conflict: true, message: result.remote.ical ? 'The event changed on the server, so your delete was not applied.' : 'The event was already removed on the server.' };
      }
      return { ok: false, conflict: false, error: result.error };
    }

    const href = remoteHref || `${collectionHref}${encodeURIComponent(uid)}.ics`;
    const result = await putCalendarEvent({ href, ical: rawIcal || '', ifMatchEtag: remoteHref ? remoteEtag : null, ...creds });
    if (result.ok) {
      await query('UPDATE calendar_events SET remote_href = $1, remote_etag = $2 WHERE calendar_id = $3 AND uid = $4 AND recurrence_id = $5', [encrypt(href), result.etag, calendar.id, uid, '']);
      return { ok: true };
    }
    if (result.conflict) {
      await applyRemoteConflict(calendar.id, calendar.user_id, uid, href, result.remote);
      return { ok: false, conflict: true, message: 'The server had a newer version of this event, so your change was not saved. The event now shows the server’s version.' };
    }
    return { ok: false, conflict: false, error: result.error };
  } catch (caught) {
    const error = toAppError(caught);
    return { ok: false, conflict: false, error: error.message || 'The remote calendar could not be reached' };
  }
}
