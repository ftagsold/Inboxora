import { calendarResources } from '../utils/calendarRecurrence.js';
import { requireCompleteMultistatus, decodeDavCharRefs } from '../utils/davXml.js';
// Pull external CalDAV/iCalendar sources. ical_url sources are always read-only
// (static feeds). CalDAV sources are read-only by default and may opt into
// two-way sync (see externalCalendarWriteback.ts) once the user marks them
// writable. Each source's sync failures are recorded per source so one
// unavailable server cannot block others.
import crypto from 'crypto';
import { XMLParser } from 'fast-xml-parser';
import { query } from './db.js';
import { decrypt } from './encryption.js';
import { safeFetch } from './safeFetch.js';
import { getConnectionPolicy } from './connectionPolicy.js';
import { parseCalendarEvent } from '../utils/ical.js';
import { toAppError } from '../utils/errors.js';
import { hrefHash } from '../utils/hrefHash.js';

export { hrefHash };

/** A decrypted external calendar source row. */
interface CalendarSyncState { removed?: boolean; promise?: Promise<unknown>; controller?: AbortController }

interface ExternalCalendarSource {
  id?: string;
  kind?: string;
  url?: string;
  username?: string | null;
  password?: string | null;
  user_id?: string;
  interval_seconds?: number;
  collections?: string | null;
  writable?: boolean;
  display_name?: string | null;
  color?: string | null;
  [key: string]: unknown;
}

/** One remote calendar collection to sync into its own local calendar. `legacy: true`
 * means the source predates discovery (or never ran it) — it keeps the original
 * `source:<id>` calendar key so nothing re-creates/duplicates existing calendars. */
export interface SourceCollection { href: string; hrefHash: string; displayName: string; color: string | null; selected: boolean; legacy?: boolean }

/** The subset of the connection policy these fetches read. */
type ExternalCalendarPolicy = { allowPrivateHosts?: boolean; [key: string]: unknown };

/** Fetch options with plain-string headers (this module owns every header it sends). */
type ExternalFetchOptions = Omit<RequestInit, 'headers'> & { headers?: Record<string, string> };

const parser = new XMLParser({ ignoreAttributes: false, removeNSPrefix: true, trimValues: false });
const syncing = new Set<string>();
const timers = new Map<string, NodeJS.Timeout>();
const inFlight = new Map<string, CalendarSyncState>();
const stopped = new Set<string>();
const toArray = <T>(value: T | T[] | null | undefined): T[] => (Array.isArray(value) ? value : value == null ? [] : [value]);
const textOf = (value: unknown): string => typeof value === 'string' ? value : String((value as Record<string, unknown> | null | undefined)?.['#text'] ?? '');
const basicAuth = (username: string, password: string): string => `Basic ${Buffer.from(`${username}:${password}`).toString('base64')}`;
function absoluteHref(href: string, baseUrl: string): string | null {
  if (!href) return null;
  try { return new URL(href, baseUrl).href; } catch { return null; }
}

// Discovered calendars are stored as one encrypted JSON blob (matching the
// whole-value encryption already used for `url`/`password`). NULL means
// discovery has never run for this source, which keeps every pre-existing
// source on the single-collection path it always used.
export function parseSourceCollections(source: ExternalCalendarSource): SourceCollection[] | null {
  if (!source.collections) return null;
  const decrypted = decrypt(source.collections);
  if (!decrypted) return null;
  try {
    const list = JSON.parse(decrypted);
    return Array.isArray(list) ? list : null;
  } catch { return null; }
}

// The collections to actually sync for a source: the discovered+selected list
// when discovery has run, otherwise a single synthetic collection built from the
// source's own `url` — today's original single-calendar behaviour, unchanged.
function collectionsToSync(source: ExternalCalendarSource): SourceCollection[] {
  const discovered = source.kind === 'caldav' ? parseSourceCollections(source) : null;
  if (discovered) return discovered.filter(collection => collection.selected);
  const url = decrypt(source.url ?? '');
  if (!url) return [];
  return [{ href: url, hrefHash: hrefHash(url), displayName: source.display_name || '', color: source.color ?? null, selected: true, legacy: true }];
}

function calendarPayloads(raw: unknown): string[] {
  if (typeof raw !== 'string' || !raw.trim()) {
    throw new Error('Remote calendar did not contain any VEVENT components');
  }
  const lineBreak = '(?:\\r\\n|\\n|\\r)';
  const eventBlocks = raw.match(new RegExp(`BEGIN:VEVENT${lineBreak}[\\s\\S]*?END:VEVENT`, 'gi')) || [];
  // TZID values in VEVENT are meaningful only with their VTIMEZONE context.
  // Keep that context alongside every independently stored event while leaving
  // unrelated components (VTODO/VJOURNAL/VFREEBUSY) out of the event resource.
  const timeZoneBlocks = raw.match(new RegExp(`BEGIN:VTIMEZONE${lineBreak}[\\s\\S]*?END:VTIMEZONE`, 'gi')) || [];
  const context = timeZoneBlocks.length ? `${timeZoneBlocks.join('\r\n')}\r\n` : '';
  try { return calendarResources(raw); } catch {
    if (!eventBlocks.length) throw new Error('Remote calendar contains an unsupported event');
    return eventBlocks.map((block) => `BEGIN:VCALENDAR\r\nVERSION:2.0\r\n${context}${block}\r\nEND:VCALENDAR\r\n`);
  }
}
function propsOf(response: { propstat?: Record<string, unknown> | Array<Record<string, unknown>> }): Record<string, unknown> {
  return toArray(response.propstat).reduce((result, propstat) => {
    if (!propstat.status || /\b2\d\d\b/.test(textOf(propstat.status))) Object.assign(result, propstat.prop || {});
    return result;
  }, {});
}

async function remoteFetch(source: ExternalCalendarSource, url: string, options: ExternalFetchOptions, policy: ExternalCalendarPolicy, signal: AbortSignal | null | undefined, secretSink?: string[]): Promise<string> {
  const headers: Record<string, string> = { ...options.headers };
  if (source.kind === 'caldav') {
    const password = decrypt(source.password ?? '');
    if (!password) throw new Error('Stored calendar source password is unavailable');
    headers.Authorization = basicAuth(source.username ?? '', password);
  }
  if (!url) throw new Error('Stored calendar source URL is unavailable');
  secretSink?.push(url);
  const timeout = AbortSignal.timeout(30_000);
  const requestSignal = signal ? AbortSignal.any([signal, timeout]) : timeout;
  const response = await safeFetch(url, { ...options, headers, redirect: 'follow', signal: requestSignal }, { allowPrivate: policy.allowPrivateHosts });
  if (!response.ok && response.status !== 207) throw new Error(`Remote calendar request failed (${response.status})`);
  return response.text();
}

/** One parsed remote event plus the resource identity (href/etag) it came from,
 * captured so a writable source can PUT/DELETE back to the right resource. */
interface FetchedEvents { payloads: Array<{ ical: string; href: string | null; etag: string | null }>; sourceDocument: string | null }

async function fetchEvents(source: ExternalCalendarSource, collectionUrl: string, policy: ExternalCalendarPolicy, signal: AbortSignal | null | undefined, secretSink?: string[]): Promise<FetchedEvents> {
  if (source.kind === 'ical_url') {
    const sourceDocument = await remoteFetch(source, collectionUrl, { headers: { Accept: 'text/calendar' } }, policy, signal, secretSink);
    return { payloads: calendarPayloads(sourceDocument).map(ical => ({ ical, href: null, etag: null })), sourceDocument };
  }
  const body = `<?xml version="1.0" encoding="utf-8"?><C:calendar-query xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><prop><getetag/><C:calendar-data/></prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"/></C:comp-filter></C:filter></C:calendar-query>`;
  const rawXml = await remoteFetch(source, collectionUrl, { method: 'REPORT', headers: { 'Content-Type': 'application/xml; charset=utf-8', Depth: '1' }, body }, policy, signal, secretSink);
  const xml = parser.parse(rawXml);
  requireCompleteMultistatus(rawXml, xml);
  const payloads: FetchedEvents['payloads'] = [];
  for (const response of toArray(xml?.multistatus?.response)) {
    const props = propsOf(response);
    const ical = decodeDavCharRefs(textOf(props['calendar-data']));
    if (!ical) continue;
    const hrefText = textOf(response.href) || (typeof response.href === 'string' ? response.href : '');
    payloads.push({ ical, href: absoluteHref(hrefText, collectionUrl), etag: (textOf(props.getetag) || '').replace(/"/g, '') || null });
  }
  return { payloads, sourceDocument: null };
}

function throwIfRemoved(state: CalendarSyncState): void {
  if (state.removed) throw new Error('Calendar source removed');
}

async function calendarFor(source: ExternalCalendarSource, collection: SourceCollection, state: CalendarSyncState) {
  const externalUrl = collection.legacy ? `source:${source.id}` : `source:${source.id}:${collection.hrefHash}`;
  const found = await query('SELECT id FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND external_url = $2', [source.user_id, externalUrl]);
  throwIfRemoved(state);
  if (found.rows[0]) return found.rows[0].id;
  const baseName = collection.legacy ? source.display_name : (collection.displayName || source.display_name);
  const color = collection.legacy ? source.color : (collection.color || source.color);
  for (let attempt = 0; attempt < 20; attempt++) {
    throwIfRemoved(state);
    const name = attempt ? `${baseName} (${attempt + 1})` : baseName;
    try {
      const inserted = await query(
        `INSERT INTO calendars (user_id, owner_user_id, name, color, source, external_url, read_only)
         VALUES ($1, $1, $2, $3, $4, $5, $6) RETURNING id`,
        [source.user_id, name, color, source.kind, externalUrl, !source.writable],
      );
      return inserted.rows[0].id;
    } catch (caught) {
      const error = toAppError(caught);
      if (error.code !== '23505') throw error;
    }
  }
  throw new Error(`Could not create a calendar for "${baseName}"`);
}

async function syncCollection(source: ExternalCalendarSource, collection: SourceCollection, policy: ExternalCalendarPolicy, state: CalendarSyncState, outboundSecrets: string[]) {
  const { payloads, sourceDocument } = await fetchEvents(source, collection.href, policy, state.controller?.signal, outboundSecrets);
  throwIfRemoved(state);
  const parsedEvents = payloads.map((item, index) => ({ ...item, index, event: parseCalendarEvent(item.ical) }));
  const events = parsedEvents.filter(({ event }) => event);
  const skipped = parsedEvents.filter(({ event }) => !event).map(({ ical, index }) => {
    const uid = ical.match(/(?:^|\r\n|\n|\r)UID(?:;[^:]*)?:([^\r\n]*)/i)?.[1]?.trim() || `event-${index + 1}`;
    return { uid, reason: 'unsupported or malformed VEVENT' };
  });
  // A wholly unsupported response must never delete a healthy projection.
  // A validated empty collection does remove its previous projection. In a mixed response, retain only the explicitly skipped
  // UIDs; other rows are known to be absent from the feed and are stale.
  if (!events.length && skipped.length) throw new Error('Remote calendar contains an unsupported event');
  const calendarId = await calendarFor(source, collection, state);
  if (sourceDocument) {
    throwIfRemoved(state);
    await query(
      `INSERT INTO calendar_import_documents (source_id, raw_ical)
       VALUES ($1, $2)
       ON CONFLICT (source_id) DO UPDATE SET raw_ical = EXCLUDED.raw_ical, updated_at = NOW()
       WHERE calendar_import_documents.raw_ical IS DISTINCT FROM EXCLUDED.raw_ical`,
      [source.id, sourceDocument],
    );
  }
  const seen = [];
  for (const { event, href, etag } of events) {
    if (!event) continue;
    throwIfRemoved(state);
    seen.push(event.uid);
    const localEtag = crypto.createHash('sha256').update(event.raw).digest('hex');
    await query(
      `INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at, all_day, timezone, description, location, url, organizer, attendees, remote_href, remote_etag)
       VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::jsonb,$16,$17)
       ON CONFLICT (calendar_id, uid, recurrence_id) DO UPDATE SET raw_ical = EXCLUDED.raw_ical,
         etag = EXCLUDED.etag, summary = EXCLUDED.summary, starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at,
         all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone, description = EXCLUDED.description,
         location = EXCLUDED.location, url = EXCLUDED.url, organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees,
         remote_href = EXCLUDED.remote_href, remote_etag = EXCLUDED.remote_etag, updated_at = NOW()`,
      [calendarId, source.user_id, event.uid, event.raw, localEtag, event.summary, event.startsAt, event.endsAt, event.allDay, event.timeZone, event.description, event.location, event.url, event.organizer, JSON.stringify(event.attendees), href, etag],
    );
  }
  throwIfRemoved(state);
  const retainedUids = [...seen, ...skipped.map(({ uid }) => uid)];
  // A sync's REPORT is a snapshot of the remote collection at the moment it was
  // fetched. An event created locally (POST /calendar/events, pushed to the
  // remote synchronously before responding) can finish after that snapshot was
  // taken but before this reconciliation delete runs — that event is real and
  // was just pushed, not stale, yet it won't appear in `retainedUids` this
  // round. Excluding rows younger than this grace window keeps them alive long
  // enough for the *next* sync's REPORT (which will include them) to confirm
  // them permanently; without it, the very next edit/delete on that event 404s
  // as "not found" because reconciliation already removed it out from under
  // the client that just created it.
  //
  // On a two-way CalDAV calendar a row without a remote_href was created here and has not
  // reached the server yet (its upload failed or is still pending). It is missing from the
  // REPORT because the server never had it, not because it was deleted there, so it is
  // kept and uploaded again below instead of being removed.
  const twoWay = source.kind === 'caldav' && Boolean(source.writable);
  await query(
    `DELETE FROM calendar_events WHERE calendar_id = $1 AND uid <> ALL($2::text[]) AND created_at < NOW() - INTERVAL '2 minutes'
       AND (NOT $3::boolean OR remote_href IS NOT NULL)`,
    [calendarId, retainedUids.length ? retainedUids : [''], twoWay],
  );
  if (twoWay && source.user_id) await uploadPendingEvents(String(calendarId), source.user_id);
  return { eventCount: events.length, skipped };
}

// Retry the upload of events created locally that the server does not have yet. Imported
// lazily: the writeback module itself imports from this one.
async function uploadPendingEvents(calendarId: string, userId: string) {
  const pending = await query<{ uid: string; raw_ical: string }>(
    `SELECT uid, raw_ical FROM calendar_events WHERE calendar_id = $1 AND user_id = $2 AND recurrence_id = '' AND remote_href IS NULL ORDER BY created_at LIMIT 50`,
    [calendarId, userId],
  );
  if (!pending.rows.length) return;
  const calendar = (await query<{ id: string; user_id: string; source: string; read_only: boolean; external_url: string | null }>(
    'SELECT id, user_id, source, read_only, external_url FROM calendars WHERE id = $1 AND user_id = $2',
    [calendarId, userId],
  )).rows[0];
  if (!calendar) return;
  const { pushEventToRemote } = await import('./externalCalendarWriteback.js');
  for (const event of pending.rows) {
    const outcome = await pushEventToRemote({ calendar, uid: event.uid, rawIcal: event.raw_ical, action: 'put' });
    if ('ok' in outcome && !outcome.ok && !outcome.conflict) console.warn(`Uploading a calendar event to the remote calendar failed: ${outcome.error}`);
  }
}

async function syncSource(source: ExternalCalendarSource) {
  const sourceId = source.id;
  if (!sourceId) return { ok: false, error: 'Calendar source is incomplete' };
  if (syncing.has(sourceId)) return { ok: false, error: 'A sync is already in progress' };
  syncing.add(sourceId);
  const outboundSecrets: string[] = [];
  const state = { controller: new AbortController(), removed: false };
  inFlight.set(sourceId, state);
  try {
    const collections = collectionsToSync(source);
    if (!collections.length) throw new Error('Stored calendar source URL is unavailable');
    const policy = await getConnectionPolicy();
    let eventCount = 0;
    const skipped: Array<{ uid: string; reason: string }> = [];
    for (const collection of collections) {
      const result = await syncCollection(source, collection, policy, state, outboundSecrets);
      eventCount += result.eventCount;
      skipped.push(...result.skipped);
    }
    if (skipped.length) {
      throwIfRemoved(state);
      const warning = JSON.stringify({ code: 'unsupported_events', count: skipped.length, samples: skipped.slice(0, 3) });
      await query('UPDATE calendar_import_sources SET last_sync_at = NOW(), last_error = $2 WHERE id = $1', [source.id, warning]);
      return { ok: true, eventCount, skipped };
    }
    throwIfRemoved(state);
    await query('UPDATE calendar_import_sources SET last_sync_at = NOW(), last_error = NULL WHERE id = $1', [source.id]);
    return { ok: true, eventCount };
  } catch (caught) {
    const error = toAppError(caught);
    if (state.removed) return { ok: false, error: 'Calendar source removed' };
    const secrets = [source.url, ...outboundSecrets].filter((value): value is string => typeof value === 'string' && value !== '');
    const safeError = secrets.reduce((message, secret) => message.replaceAll(secret, '[redacted]'), String(error.message || 'Calendar source sync failed'));
    await query('UPDATE calendar_import_sources SET last_sync_at = NOW(), last_error = $2 WHERE id = $1', [source.id, safeError]);
    return { ok: false, error: safeError };
  } finally {
    syncing.delete(sourceId);
    inFlight.delete(sourceId);
  }
}

function runSync(source: ExternalCalendarSource) {
  const sourceId = source.id;
  if (!sourceId) return Promise.resolve({ ok: false, error: 'Calendar source is incomplete' });
  if (stopped.has(sourceId)) return Promise.resolve({ ok: false, error: 'Calendar source removed' });
  if (inFlight.has(sourceId)) return syncSource(source);
  const promise = syncSource(source);
  const state = inFlight.get(sourceId);
  if (state) state.promise = promise;
  return promise;
}

export async function syncCalendarSource(userId: string, sourceId: string) {
  const result = await query('SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND enabled = true', [sourceId, userId]);
  if (!result.rows[0]) return { ok: false, error: 'Calendar source not found' };
  return runSync(result.rows[0]);
}
export async function syncAllCalendarSources() {
  const result = await query('SELECT * FROM calendar_import_sources WHERE enabled = true');
  return Promise.allSettled(result.rows.map(runSync));
}
export function scheduleCalendarSource(source: ExternalCalendarSource): void {
  const sourceId = source.id;
  const intervalSeconds = source.interval_seconds;
  // The cron expression only ever yields a complete row; a missing interval would otherwise
  // schedule setInterval(NaN), which fires in a tight loop.
  if (!sourceId || !intervalSeconds) return;
  const previous = timers.get(sourceId); if (previous) clearInterval(previous);
  timers.set(sourceId, setInterval(() => runSync(source).catch(() => {}), intervalSeconds * 1_000));
}
export async function stopCalendarSource(id: string): Promise<void> {
  const timer = timers.get(id); if (timer) clearInterval(timer); timers.delete(id);
  const state = inFlight.get(id);
  // Only real scheduled or active sources need a tombstone. In particular,
  // DELETE of an unknown ID must not grow this process-global set forever.
  if (!timer && !state) return;
  stopped.add(id);
  if (!state) return;
  state.removed = true;
  state.controller?.abort(new Error('Calendar source removed'));
  await state.promise;
}
export function releaseCalendarSource(id: string): void {
  stopped.delete(id);
}
export async function startExternalCalendarScheduler() {
  const result = await query('SELECT * FROM calendar_import_sources WHERE enabled = true');
  result.rows.forEach(scheduleCalendarSource);
}
