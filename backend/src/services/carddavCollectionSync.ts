// CardDAV address-book sync — the contacts side of a unified DAV server row
// (calendar_import_sources, kind='caldav'). Sibling to externalCalendarSync.ts:
// same per-source-id scheduling shape, independent `address_book_*` status
// columns so a calendar-side failure never hides an address-book one (or vice
// versa). One-way / read-only, same as the CardDAV client it wraps
// (carddavClient.ts) always has been.

import crypto from 'crypto';
import { query } from './db.js';
import { decrypt } from './encryption.js';
import { parseVCard } from '../utils/vcard.js';
import { getConnectionPolicy } from './connectionPolicy.js';
import { fetchAddressBookCards } from './carddavClient.js';
import { hrefHash } from '../utils/hrefHash.js';
import { toAppError } from '../utils/errors.js';

interface CarddavSourceRow {
  id: string;
  user_id: string;
  username?: string | null;
  password?: string | null;
  address_book_collections?: string | null;
  interval_seconds?: number;
  enabled?: boolean;
  [key: string]: unknown;
}

/** One remote address book to sync into its own local, read-only address book. */
export interface AddressBookCollection { href: string; hrefHash: string; displayName: string; selected: boolean }

type CarddavSyncState = { removed?: boolean; promise?: Promise<unknown>; controller?: AbortController };

const timers = new Map<string, NodeJS.Timeout>();
const inFlight = new Map<string, CarddavSyncState>();
const syncing = new Set<string>();
const stopped = new Set<string>();

// Discovered address books are stored as one encrypted JSON blob, matching how
// calendar collections are stored on the same row. NULL means "this server has
// no address books" — there is no legacy single-book fallback to preserve, since
// every unified server row is created discovery-first.
export function parseAddressBookCollections(source: CarddavSourceRow): AddressBookCollection[] | null {
  if (!source.address_book_collections) return null;
  const decrypted = decrypt(source.address_book_collections);
  if (!decrypted) return null;
  try {
    const list = JSON.parse(decrypted);
    return Array.isArray(list) ? list : null;
  } catch { return null; }
}

function collectionsToSync(source: CarddavSourceRow): AddressBookCollection[] {
  const parsed = parseAddressBookCollections(source);
  return parsed ? parsed.filter(collection => collection.selected) : [];
}

function throwIfRemoved(state: CarddavSyncState): void {
  if (state.removed) throw new Error('DAV server removed');
}

// Find or create the local read-only address book mirroring a remote collection,
// keyed by the same `source:<sourceId>:<hrefHash>` scheme externalCalendarSync.ts
// uses for calendars — this is what makes per-server teardown and multi-server
// collision-safety possible (unlike the old single-account model, which keyed
// directly off the book's raw URL). Address-book names are unique per user, so on
// a name clash we disambiguate with a suffix.
async function ensureCardavBook(userId: string, sourceId: string, book: { href: string; displayName: string }): Promise<string> {
  const externalUrl = `source:${sourceId}:${hrefHash(book.href)}`;
  const existing = await query<{ id: string }>('SELECT id FROM address_books WHERE user_id = $1 AND external_url = $2', [userId, externalUrl]);
  if (existing.rows.length) return existing.rows[0].id;

  for (let attempt = 0; attempt < 20; attempt++) {
    const name = attempt === 0 ? book.displayName : `${book.displayName} (${attempt + 1})`;
    try {
      const inserted = await query<{ id: string }>(
        `INSERT INTO address_books (user_id, name, source, external_url) VALUES ($1, $2, 'carddav', $3) RETURNING id`,
        [userId, name, externalUrl],
      );
      return inserted.rows[0].id;
    } catch (caught) {
      const err = toAppError(caught);
      if (err.code === '23505') continue; // name taken — try next suffix
      throw err;
    }
  }
  throw new Error(`Could not create a local address book for "${book.displayName}"`);
}

function contactFromVCard(vcard: string, href: string) {
  const c = parseVCard(vcard);
  const uid = c.uid || crypto.createHash('md5').update(href).digest('hex');
  const primaryEmail = c.emails.find(e => e.primary)?.value || c.emails[0]?.value || null;
  return {
    uid,
    displayName: c.displayName || primaryEmail || null,
    firstName: c.firstName, lastName: c.lastName,
    primaryEmail: primaryEmail ? primaryEmail.toLowerCase().trim() : null,
    emails: c.emails, phones: c.phones,
    organization: c.organization, notes: c.notes, birthday: c.birthday, anniversary: c.anniversary, contactDates: c.contactDates, photoData: c.photoData,
    title: c.title, role: c.role, nickname: c.nickname, urls: c.urls,
    instantMessages: c.instantMessages, categories: c.categories, addresses: c.addresses,
    invalidDates: c.invalidDates, invalidDateLabels: c.invalidDateLabels,
    vcard,
  };
}

type CardavContact = ReturnType<typeof contactFromVCard>;
type CardavCredentials = { username: string; password: string; allowPrivate: boolean };

async function upsertCardavContact(bookId: string, userId: string, c: CardavContact) {
  const etag = crypto.createHash('md5').update(c.vcard).digest('hex');
  await query(`
    INSERT INTO contacts (
      address_book_id, user_id, uid, vcard, etag,
      display_name, first_name, last_name, primary_email,
      emails, phones, organization, notes, birthday, anniversary, contact_dates, photo_data,
      title, role, nickname, urls, instant_messages, categories, addresses, is_auto
    ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::jsonb,$11::jsonb,$12,$13,$14,$15,$16::jsonb,$17,$18,$19,$20,$21::jsonb,$22::jsonb,$23::jsonb,$24::jsonb,false)
    ON CONFLICT (address_book_id, uid) DO UPDATE SET
      vcard = EXCLUDED.vcard, etag = EXCLUDED.etag,
      display_name = EXCLUDED.display_name, first_name = EXCLUDED.first_name,
      last_name = EXCLUDED.last_name, primary_email = EXCLUDED.primary_email,
      emails = EXCLUDED.emails, phones = EXCLUDED.phones,
      organization = EXCLUDED.organization, notes = EXCLUDED.notes,
      birthday = EXCLUDED.birthday, anniversary = EXCLUDED.anniversary,
      contact_dates = EXCLUDED.contact_dates,
      title = EXCLUDED.title, role = EXCLUDED.role, nickname = EXCLUDED.nickname,
      urls = EXCLUDED.urls, instant_messages = EXCLUDED.instant_messages,
      categories = EXCLUDED.categories, addresses = EXCLUDED.addresses,
      photo_data = EXCLUDED.photo_data, updated_at = NOW()
  `, [
    bookId, userId, c.uid, c.vcard, etag,
    c.displayName, c.firstName, c.lastName, c.primaryEmail,
    JSON.stringify(c.emails), JSON.stringify(c.phones),
    c.organization, c.notes, c.birthday, c.anniversary, JSON.stringify(c.contactDates), c.photoData,
    c.title, c.role, c.nickname, JSON.stringify(c.urls), JSON.stringify(c.instantMessages), JSON.stringify(c.categories), JSON.stringify(c.addresses),
  ]);
}

// No cross-book duplicate handling (merge/skip) — every unified server's address
// books stay separate, same as CalDAV's multi-calendar sources have no such
// concept either. Still de-dupe *within* one book so two cards sharing an email
// don't violate that book's (address_book_id, primary_email) uniqueness.
async function syncBook(userId: string, sourceId: string, collection: AddressBookCollection, creds: CardavCredentials, state: CarddavSyncState) {
  const rawCards = await fetchAddressBookCards({ url: collection.href, ...creds });
  throwIfRemoved(state);
  const cards = rawCards.map(rc => contactFromVCard(rc.vcard, rc.href));
  if (cards.some(card => card.invalidDates.length || card.invalidDateLabels.length)) {
    throw new Error('Remote CardDAV vCard contains an invalid contact date');
  }
  const bookId = await ensureCardavBook(userId, sourceId, collection);
  throwIfRemoved(state);

  const seenInBook = new Set<string>();
  for (const c of cards) {
    if (c.primaryEmail && seenInBook.has(c.primaryEmail)) c.primaryEmail = null;
    else if (c.primaryEmail) seenInBook.add(c.primaryEmail);
  }

  const presentUids = cards.map(c => c.uid);
  await query('DELETE FROM contacts WHERE address_book_id = $1 AND uid <> ALL($2::text[])', [bookId, presentUids.length ? presentUids : ['']]);
  for (const c of cards) { throwIfRemoved(state); await upsertCardavContact(bookId, userId, c); }

  await query("UPDATE address_books SET sync_token = gen_random_uuid()::text, updated_at = NOW() WHERE id = $1", [bookId]);
  return { count: presentUids.length };
}

async function syncAddressBooks(source: CarddavSourceRow) {
  const sourceId = source.id;
  if (syncing.has(sourceId)) return { ok: false, error: 'A sync is already in progress' };
  syncing.add(sourceId);
  const state: CarddavSyncState = { controller: new AbortController(), removed: false };
  inFlight.set(sourceId, state);
  try {
    const collections = collectionsToSync(source);
    if (!collections.length) {
      await query('UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = NULL WHERE id = $1', [sourceId]);
      return { ok: true, bookCount: 0, contactCount: 0 };
    }
    const policy = await getConnectionPolicy();
    if (typeof source.username !== 'string') throw new Error('CardDAV username is missing');
    const decryptedPassword = decrypt(source.password ?? '');
    if (typeof decryptedPassword !== 'string') throw new Error('CardDAV password could not be decrypted');
    const creds: CardavCredentials = { username: source.username, password: decryptedPassword, allowPrivate: policy.allowPrivateHosts };

    let contactCount = 0;
    const seenExternalUrls: string[] = [];
    for (const collection of collections) {
      throwIfRemoved(state);
      const { count } = await syncBook(source.user_id, sourceId, collection, creds, state);
      contactCount += count;
      seenExternalUrls.push(`source:${sourceId}:${collection.hrefHash}`);
    }
    throwIfRemoved(state);
    // Prune address books whose collection was deselected or vanished from discovery,
    // scoped to this source so it never touches another server's books.
    await query(
      `DELETE FROM address_books WHERE user_id = $1 AND source = 'carddav' AND external_url LIKE $2 AND external_url <> ALL($3::text[])`,
      [source.user_id, `source:${sourceId}:%`, seenExternalUrls.length ? seenExternalUrls : ['']],
    );
    await query('UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = NULL WHERE id = $1', [sourceId]);
    return { ok: true, bookCount: collections.length, contactCount };
  } catch (caught) {
    const err = toAppError(caught);
    if (state.removed) return { ok: false, error: 'DAV server removed' };
    await query('UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = $2 WHERE id = $1', [sourceId, err.message]);
    return { ok: false, error: err.message };
  } finally {
    syncing.delete(sourceId);
    inFlight.delete(sourceId);
  }
}

function runSync(source: CarddavSourceRow) {
  const sourceId = source.id;
  if (!sourceId) return Promise.resolve({ ok: false, error: 'DAV server is incomplete' });
  if (stopped.has(sourceId)) return Promise.resolve({ ok: false, error: 'DAV server removed' });
  if (inFlight.has(sourceId)) return syncAddressBooks(source);
  const promise = syncAddressBooks(source);
  const state = inFlight.get(sourceId);
  if (state) state.promise = promise;
  return promise;
}

export async function syncCarddavSide(userId: string, sourceId: string) {
  const result = await query<CarddavSourceRow>('SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND enabled = true', [sourceId, userId]);
  if (!result.rows[0]) return { ok: false, error: 'DAV server not found' };
  return runSync(result.rows[0]);
}

export function scheduleCarddavSide(source: CarddavSourceRow): void {
  const sourceId = source.id;
  if (!sourceId) return;
  const previous = timers.get(sourceId); if (previous) clearInterval(previous);
  timers.delete(sourceId);
  // Nothing to schedule if this server has no address-book side at all, or no
  // valid cadence — guards setInterval(NaN), which would fire in a tight loop.
  if (!source.interval_seconds || !source.address_book_collections) return;
  timers.set(sourceId, setInterval(() => runSync(source).catch(() => {}), source.interval_seconds! * 1_000));
}

export async function stopCarddavSide(id: string): Promise<void> {
  const timer = timers.get(id); if (timer) clearInterval(timer); timers.delete(id);
  const state = inFlight.get(id);
  if (!timer && !state) return;
  stopped.add(id);
  if (!state) return;
  state.removed = true;
  state.controller?.abort(new Error('DAV server removed'));
  await state.promise;
}

export function releaseCarddavSide(id: string): void {
  stopped.delete(id);
}

export async function startCarddavSchedulers() {
  const result = await query<CarddavSourceRow>("SELECT * FROM calendar_import_sources WHERE enabled = true AND address_book_collections IS NOT NULL");
  result.rows.forEach(scheduleCarddavSide);
}
