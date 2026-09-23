// Unified CalDAV + CardDAV server manager. One remote server (one set of
// credentials) is one `calendar_import_sources` row (kind='caldav'), with an
// optional calendar side (`collections`, synced by externalCalendarSync.ts) and
// an optional address-book side (`address_book_collections`, synced by
// carddavCollectionSync.ts) — either or both, discovered and selected together
// from a single form. Plain ICS/webcal subscriptions (`kind='ical_url'`) are a
// different, authless feature and stay on routes/calendar.ts's generic source
// CRUD, managed from CalendarSubscriptionsSettings.tsx.

import { Router } from 'express';
import crypto from 'crypto';
import { query } from '../services/db.js';
import { requireAuth } from '../middleware/auth.js';
import { decrypt, encrypt } from '../services/encryption.js';
import { getConnectionPolicy } from '../services/connectionPolicy.js';
import { validateSourceUrl, resolveCaldavUrl, clampIntervalSeconds, validatedCollectionHref } from '../services/davSourceUrl.js';
import type { IncomingCollection } from '../services/davSourceUrl.js';
import { discoverCalendars } from '../services/caldavClient.js';
import { discoverAddressBooks } from '../services/carddavClient.js';
import { releaseCalendarSource, scheduleCalendarSource, stopCalendarSource, syncCalendarSource, parseSourceCollections } from '../services/externalCalendarSync.js';
import type { SourceCollection } from '../services/externalCalendarSync.js';
import { releaseCarddavSide, scheduleCarddavSide, stopCarddavSide, syncCarddavSide, parseAddressBookCollections } from '../services/carddavCollectionSync.js';
import type { AddressBookCollection } from '../services/carddavCollectionSync.js';
import { sessionUserId } from '../utils/query.js';
import { toAppError } from '../utils/errors.js';

const router = Router();
router.use(requireAuth);

interface DavServerRow {
  id: string;
  user_id: string;
  kind?: string;
  url?: string;
  username?: string | null;
  password?: string | null;
  display_name?: string | null;
  interval_seconds?: number;
  writable?: boolean;
  enabled?: boolean;
  collections?: string | null;
  address_book_collections?: string | null;
  last_sync_at?: string | Date | null;
  last_error?: string | null;
  address_book_last_sync_at?: string | Date | null;
  address_book_last_error?: string | null;
  [key: string]: unknown;
}

function publicServer(source: DavServerRow) {
  const decryptedUrl = source.url ? decrypt(source.url) : null;
  const secretValues = (decryptedUrl === null ? (source.url ? [source.url] : []) : [source.url, decryptedUrl]).filter((value): value is string => Boolean(value));
  const redact = (message: unknown) => typeof message === 'string' ? secretValues.reduce((m, s) => m.replaceAll(s, '[redacted]'), message) : message;
  const calendars = parseSourceCollections(source);
  const addressBooks = parseAddressBookCollections(source);
  return {
    id: source.id,
    displayName: source.display_name,
    username: source.username,
    intervalSeconds: source.interval_seconds,
    writable: Boolean(source.writable),
    enabled: source.enabled,
    calendarSync: { at: source.last_sync_at ?? null, error: redact(source.last_error) },
    addressBookSync: { at: source.address_book_last_sync_at ?? null, error: redact(source.address_book_last_error) },
    // Collection hrefs never leave the server — only an opaque id (the href hash)
    // the client sends back to select/deselect it, same secrecy as `url`.
    calendars: calendars ? calendars.map(c => ({ id: c.hrefHash, displayName: c.displayName, color: c.color, selected: c.selected })) : null,
    addressBooks: addressBooks ? addressBooks.map(c => ({ id: c.hrefHash, displayName: c.displayName, selected: c.selected })) : null,
  };
}

router.get('/', async (req, res) => {
  const result = await query<DavServerRow>(
    `SELECT id, kind, url, username, display_name, interval_seconds, writable, collections, address_book_collections, enabled, last_sync_at, last_error, address_book_last_sync_at, address_book_last_error
     FROM calendar_import_sources WHERE user_id = $1 AND kind = 'caldav' ORDER BY created_at ASC`,
    [req.session.userId],
  );
  res.json({ servers: result.rows.map(publicServer) });
});

// Discover both the calendars and the address books a server exposes for these
// credentials, without persisting anything. A server may speak only one
// protocol (or neither) — a 502 is only returned when both fail; otherwise the
// successful side's results come back alongside the failed side's error, so the
// client can show partial results rather than lose the whole discovery.
router.post('/discover', async (req, res) => {
  const { url, username, password } = req.body || {};
  if (!username || !password) return res.status(400).json({ error: 'username and password are required' });
  const resolved = resolveCaldavUrl(url, username);
  if ('error' in resolved) return res.status(400).json({ error: resolved.error });
  const validated = await validateSourceUrl(resolved.url);
  if ('error' in validated) return res.status(400).json({ error: validated.error });
  const serverUrl = validated.url.toString();
  const allowPrivate = validated.policy.allowPrivateHosts;

  const [calendarsResult, addressBooksResult] = await Promise.allSettled([
    discoverCalendars({ serverUrl, username, password, allowPrivate }),
    discoverAddressBooks({ serverUrl, username, password, allowPrivate }),
  ]);
  const response: { calendars?: unknown; calendarsError?: string; addressBooks?: unknown; addressBooksError?: string } = {};
  if (calendarsResult.status === 'fulfilled') response.calendars = calendarsResult.value;
  else response.calendarsError = toAppError(calendarsResult.reason).message || 'Calendar discovery failed';
  if (addressBooksResult.status === 'fulfilled') response.addressBooks = addressBooksResult.value.map(book => ({ href: book.url, displayName: book.displayName }));
  else response.addressBooksError = toAppError(addressBooksResult.reason).message || 'Address book discovery failed';

  if (calendarsResult.status === 'rejected' && addressBooksResult.status === 'rejected') {
    return res.status(502).json({ error: 'No calendars or address books found for this account', ...response });
  }
  res.json(response);
});

router.post('/', async (req, res) => {
  const { url, username, password, displayName, intervalSeconds, writable = false, calendars: incomingCalendars, addressBooks: incomingAddressBooks } = req.body || {};
  if (!displayName || !username || !password) return res.status(400).json({ error: 'displayName, username and password are required' });
  const hasCalendars = Array.isArray(incomingCalendars) && incomingCalendars.length > 0;
  const hasAddressBooks = Array.isArray(incomingAddressBooks) && incomingAddressBooks.length > 0;
  if (!hasCalendars && !hasAddressBooks) return res.status(400).json({ error: 'Select at least one calendar or address book to import' });

  const resolvedUrl = resolveCaldavUrl(url, username);
  if ('error' in resolvedUrl) return res.status(400).json({ error: resolvedUrl.error });
  const validated = await validateSourceUrl(resolvedUrl.url);
  if ('error' in validated) return res.status(400).json({ error: validated.error });
  const { url: parsed, policy } = validated;

  const calendarCollections: SourceCollection[] = [];
  if (Array.isArray(incomingCalendars)) {
    for (const entry of incomingCalendars as IncomingCollection[]) {
      const validatedHref = await validatedCollectionHref(entry?.href, policy);
      if ('error' in validatedHref) return res.status(400).json({ error: validatedHref.error });
      calendarCollections.push({ ...validatedHref, displayName: entry.displayName || 'Calendar', color: entry.color || null, selected: entry.selected !== false });
    }
  }
  const addressBookCollections: AddressBookCollection[] = [];
  if (Array.isArray(incomingAddressBooks)) {
    for (const entry of incomingAddressBooks as IncomingCollection[]) {
      const validatedHref = await validatedCollectionHref(entry?.href, policy);
      if ('error' in validatedHref) return res.status(400).json({ error: validatedHref.error });
      addressBookCollections.push({ ...validatedHref, displayName: entry.displayName || 'Contacts', selected: entry.selected !== false });
    }
  }

  const interval = clampIntervalSeconds(intervalSeconds);
  try {
    const normalizedUrl = parsed.toString();
    const urlFingerprint = crypto.createHash('sha256').update(normalizedUrl).digest('hex');
    const result = await query<DavServerRow>(
      `INSERT INTO calendar_import_sources (user_id, kind, url, url_fingerprint, username, password, display_name, interval_seconds, writable, collections, address_book_collections)
       VALUES ($1,'caldav',$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *`,
      [
        req.session.userId, encrypt(normalizedUrl), urlFingerprint, username, encrypt(password), displayName, interval, Boolean(writable),
        encrypt(JSON.stringify(calendarCollections)),
        Array.isArray(incomingAddressBooks) ? encrypt(JSON.stringify(addressBookCollections)) : null,
      ],
    );
    const source = result.rows[0];
    scheduleCalendarSource(source);
    scheduleCarddavSide(source);
    const [calendarSync, addressBookSync] = await Promise.all([
      calendarCollections.length ? syncCalendarSource(sessionUserId(req), source.id) : Promise.resolve({ ok: true as const }),
      addressBookCollections.length ? syncCarddavSide(sessionUserId(req), source.id) : Promise.resolve({ ok: true as const }),
    ]);
    if (!calendarSync.ok || !addressBookSync.ok) {
      // The syncs record their failure asynchronously from the insert result;
      // reflect that terminal state in the response so the client can render
      // the persisted server as retryable immediately, same as calendar sources do.
      if (!calendarSync.ok) source.last_error = calendarSync.error;
      if (!addressBookSync.ok) source.address_book_last_error = addressBookSync.error;
      const error = (!calendarSync.ok && calendarSync.error) || (!addressBookSync.ok && addressBookSync.error) || 'Sync failed';
      return res.status(502).json({ error, server: publicServer(source), calendarSync, addressBookSync });
    }
    res.status(201).json({ server: publicServer(source), calendarSync, addressBookSync });
  } catch (caught) {
    const error = toAppError(caught);
    if (error.code === '23505') return res.status(409).json({ error: 'A server with this URL already exists' });
    if (error.code === '23514') return res.status(409).json({ error: 'Server URL could not be stored securely' });
    throw error;
  }
});

router.post('/:id/sync', async (req, res) => {
  const [calendarSync, addressBookSync] = await Promise.all([
    syncCalendarSource(sessionUserId(req), req.params.id),
    syncCarddavSide(sessionUserId(req), req.params.id),
  ]);
  const notFound = (result: { ok: boolean; error?: string }) => !result.ok && (result.error === 'Calendar source not found' || result.error === 'DAV server not found');
  if (notFound(calendarSync) && notFound(addressBookSync)) return res.status(404).json({ error: 'DAV server not found' });
  res.json({ calendarSync, addressBookSync });
});

// Every field is optional and independently editable — name, credentials,
// cadence, the writable opt-in, and which discovered calendars/address books
// are imported (full replace after a re-discover, or a selection-only toggle).
router.patch('/:id', async (req, res) => {
  const existingResult = await query<DavServerRow>(`SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND kind = 'caldav'`, [req.params.id, req.session.userId]);
  const existing = existingResult.rows[0];
  if (!existing) return res.status(404).json({ error: 'DAV server not found' });

  const body = req.body || {};
  const sets: string[] = [];
  const values: unknown[] = [];
  const push = (column: string, value: unknown) => { values.push(value); sets.push(`${column} = $${values.length}`); };

  if (body.displayName !== undefined) {
    if (typeof body.displayName !== 'string' || !body.displayName.trim()) return res.status(400).json({ error: 'displayName must not be empty' });
    push('display_name', body.displayName.trim());
  }
  if (body.intervalSeconds !== undefined) {
    const interval = Number.parseInt(body.intervalSeconds, 10);
    if (!Number.isInteger(interval) || interval < 15 || interval > 86400) {
      return res.status(400).json({ error: 'intervalSeconds must be between 15 and 86400' });
    }
    push('interval_seconds', interval);
  }
  if (body.enabled !== undefined) push('enabled', Boolean(body.enabled));
  if (body.username) push('username', body.username);
  if (body.password) push('password', encrypt(body.password)); // blank/omitted keeps the stored password
  let writableChanged = false;
  if (body.writable !== undefined) { push('writable', Boolean(body.writable)); writableChanged = true; }

  const connectionPolicy = await getConnectionPolicy();

  let nextCalendarCollections: SourceCollection[] | null = null;
  if (Array.isArray(body.calendars)) {
    const incoming = body.calendars as IncomingCollection[];
    if (incoming.some(entry => entry?.href)) {
      const built: SourceCollection[] = [];
      for (const entry of incoming) {
        const validatedHref = await validatedCollectionHref(entry?.href, connectionPolicy);
        if ('error' in validatedHref) return res.status(400).json({ error: validatedHref.error });
        built.push({ ...validatedHref, displayName: entry.displayName || 'Calendar', color: entry.color || null, selected: entry.selected !== false });
      }
      nextCalendarCollections = built;
    } else {
      const current = parseSourceCollections(existing) || [];
      const toggles = new Map(incoming.filter(entry => entry?.id).map(entry => [entry.id, Boolean(entry.selected)]));
      nextCalendarCollections = current.map(collection => toggles.has(collection.hrefHash) ? { ...collection, selected: toggles.get(collection.hrefHash)! } : collection);
    }
    push('collections', encrypt(JSON.stringify(nextCalendarCollections)));
  }
  let nextAddressBookCollections: AddressBookCollection[] | null = null;
  if (Array.isArray(body.addressBooks)) {
    const incoming = body.addressBooks as IncomingCollection[];
    if (incoming.some(entry => entry?.href)) {
      const built: AddressBookCollection[] = [];
      for (const entry of incoming) {
        const validatedHref = await validatedCollectionHref(entry?.href, connectionPolicy);
        if ('error' in validatedHref) return res.status(400).json({ error: validatedHref.error });
        built.push({ ...validatedHref, displayName: entry.displayName || 'Contacts', selected: entry.selected !== false });
      }
      nextAddressBookCollections = built;
    } else {
      const current = parseAddressBookCollections(existing) || [];
      const toggles = new Map(incoming.filter(entry => entry?.id).map(entry => [entry.id, Boolean(entry.selected)]));
      nextAddressBookCollections = current.map(collection => toggles.has(collection.hrefHash) ? { ...collection, selected: toggles.get(collection.hrefHash)! } : collection);
    }
    push('address_book_collections', encrypt(JSON.stringify(nextAddressBookCollections)));
  }

  if (!sets.length) return res.status(400).json({ error: 'No editable fields were provided' });
  values.push(req.params.id, req.session.userId);
  const result = await query<DavServerRow>(
    `UPDATE calendar_import_sources SET ${sets.join(', ')}, updated_at = NOW()
     WHERE id = $${values.length - 1} AND user_id = $${values.length} RETURNING *`,
    values,
  );
  const source = result.rows[0];
  if (!source) return res.status(404).json({ error: 'DAV server not found' });

  // Re-arm both timers — the schedulers close over the row they were given, so
  // without this a new interval, pause/resume, or a newly-added protocol side
  // would not take effect until restart.
  if (source.enabled) {
    scheduleCalendarSource(source);
    scheduleCarddavSide(source);
  } else {
    await stopCalendarSource(source.id); releaseCalendarSource(source.id);
    await stopCarddavSide(source.id); releaseCarddavSide(source.id);
  }

  // A calendar's writability lives on its own `calendars` row (read_only); flip
  // every row derived from this source to match.
  if (writableChanged) {
    await query(
      `UPDATE calendars SET read_only = $1, updated_at = NOW()
       WHERE user_id = $2 AND owner_user_id = $2 AND (external_url = $3 OR external_url LIKE $3 || ':%')`,
      [!source.writable, req.session.userId, `source:${source.id}`],
    );
  }
  // A calendar/address book deselected from a multi-collection server is removed
  // locally — there's no dedicated "remove one imported calendar" route, and
  // leaving an orphaned, never-synced-again row around would be a confusing dead end.
  if (nextCalendarCollections) {
    const deselected = nextCalendarCollections.filter(collection => !collection.selected).map(collection => `source:${source.id}:${collection.hrefHash}`);
    if (deselected.length) await query('DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND external_url = ANY($2::text[])', [req.session.userId, deselected]);
  }
  if (nextAddressBookCollections) {
    const deselected = nextAddressBookCollections.filter(collection => !collection.selected).map(collection => `source:${source.id}:${collection.hrefHash}`);
    if (deselected.length) await query(`DELETE FROM address_books WHERE user_id = $1 AND source = 'carddav' AND external_url = ANY($2::text[])`, [req.session.userId, deselected]);
  }
  res.json({ server: publicServer(source) });
});

router.delete('/:id', async (req, res) => {
  const existing = await query<DavServerRow>(`SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND kind = 'caldav'`, [req.params.id, req.session.userId]);
  if (!existing.rows[0]) return res.status(404).json({ error: 'DAV server not found' });
  let sourceDeleted = false;
  try {
    await stopCalendarSource(req.params.id);
    await stopCarddavSide(req.params.id);
    const result = await query('DELETE FROM calendar_import_sources WHERE id = $1 AND user_id = $2 RETURNING id', [req.params.id, req.session.userId]);
    if (!result.rows[0]) {
      releaseCalendarSource(req.params.id); releaseCarddavSide(req.params.id);
      return res.status(404).json({ error: 'DAV server not found' });
    }
    sourceDeleted = true;
    // A multi-collection server's derived rows carry a hash suffix (source:<id>:<hash>);
    // a single-collection (or pre-discovery) legacy calendar source keeps the plain
    // source:<id> key — mirrored here for calendars the same way calendar.ts's own
    // DELETE does.
    await query(
      `DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND (external_url = $2 OR external_url LIKE $2 || ':%')`,
      [req.session.userId, `source:${req.params.id}`],
    );
    await query(`DELETE FROM address_books WHERE user_id = $1 AND source = 'carddav' AND external_url LIKE $2`, [req.session.userId, `source:${req.params.id}:%`]);
    releaseCalendarSource(req.params.id); releaseCarddavSide(req.params.id);
    res.status(204).end();
  } catch (error) {
    releaseCalendarSource(req.params.id); releaseCarddavSide(req.params.id);
    if (!sourceDeleted) { scheduleCalendarSource(existing.rows[0]); scheduleCarddavSide(existing.rows[0]); }
    throw error;
  }
});

export default router;
