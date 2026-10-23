// Browser cache of mail (IndexedDB, one database per signed-in user): display fields by
// message id, rendered bodies, and the rows of recently opened list views. A message's display
// fields and body never change, so they are served from here without asking the server; list
// views are shown at once and then revalidated. Everything is deleted on sign-out.
//
// Every call is best-effort: without IndexedDB (private windows, tests) reads miss and writes
// do nothing.

import type { StoreMessageRow } from '../store/index.ts';

const DB_PREFIX = 'inboxora-mail-';
const SCHEMA = 1;
/** Bodies kept per user; the least recently read go first. */
export const BODY_CAP = 2000;
/** Bodies are re-fetched after this long (matches the server's body cache). */
export const BODY_MAX_AGE_MS = 7 * 24 * 3600 * 1000;
/** Rows stored per list view. */
export const VIEW_ROWS = 200;

/** Display fields a list row carries (what `meta_pending` rows lack). */
export const META_FIELDS = [
  'subject', 'from_name', 'from_email', 'sender_name', 'sender_email', 'to_addresses', 'cc_addresses',
  'reply_to', 'in_reply_to', 'thread_references', 'snippet', 'list_unsubscribe', 'list_unsubscribe_post',
  'delivery_addresses', 'has_contact_photo',
] as const;

export type MetaFields = Partial<Record<(typeof META_FIELDS)[number], unknown>>;

export interface CachedView {
  rows: StoreMessageRow[];
  total: number;
  seq: number;
  at: number;
}

let userId: string | null = null;
let dbPromise: Promise<IDBDatabase | null> | null = null;

function idb(): IDBFactory | null {
  try {
    return typeof indexedDB === 'undefined' ? null : indexedDB;
  } catch {
    return null;
  }
}

/** Point the cache at the signed-in user (null on sign-out, which deletes the old user's data). */
export function setMailCacheUser(next: string | null): void {
  if (next === userId) return;
  const previous = userId;
  userId = next;
  const old = dbPromise;
  dbPromise = null;
  if (old) void old.then(db => db?.close()).catch(() => {});
  if (previous && !next) void deleteDatabase(previous);
}

function deleteDatabase(id: string): Promise<void> {
  return new Promise(resolve => {
    const factory = idb();
    if (!factory) return resolve();
    try {
      const req = factory.deleteDatabase(DB_PREFIX + id);
      req.onsuccess = req.onerror = req.onblocked = () => resolve();
    } catch {
      resolve();
    }
  });
}

/** Delete the cache of the current (or given) user. */
export async function wipeMailCache(id: string | null = userId): Promise<void> {
  if (!id) return;
  if (id === userId && dbPromise) {
    const db = await dbPromise.catch(() => null);
    db?.close();
    dbPromise = null;
  }
  await deleteDatabase(id);
}

function open(): Promise<IDBDatabase | null> {
  if (!userId) return Promise.resolve(null);
  if (dbPromise) return dbPromise;
  const factory = idb();
  if (!factory) return Promise.resolve(null);
  const name = DB_PREFIX + userId;
  dbPromise = new Promise<IDBDatabase | null>(resolve => {
    try {
      const req = factory.open(name, SCHEMA);
      req.onupgradeneeded = () => {
        const db = req.result;
        for (const store of Array.from(db.objectStoreNames)) db.deleteObjectStore(store);
        db.createObjectStore('meta');
        const bodies = db.createObjectStore('bodies');
        bodies.createIndex('at', 'at');
        db.createObjectStore('views');
      };
      req.onsuccess = () => {
        const db = req.result;
        db.onversionchange = () => { db.close(); dbPromise = null; };
        resolve(db);
      };
      req.onerror = req.onblocked = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
  // Ask the browser not to evict the cache under storage pressure (best-effort).
  try { void navigator.storage?.persist?.(); } catch { /* unsupported */ }
  return dbPromise;
}

function done(tx: IDBTransaction): Promise<void> {
  return new Promise(resolve => {
    tx.oncomplete = tx.onerror = tx.onabort = () => resolve();
  });
}

function read<T>(store: string, keys: string[]): Promise<(T | undefined)[]> {
  return open().then(db => {
    if (!db || !keys.length) return keys.map(() => undefined);
    return new Promise<(T | undefined)[]>(resolve => {
      try {
        const tx = db.transaction(store, 'readonly');
        const os = tx.objectStore(store);
        const out: (T | undefined)[] = new Array(keys.length);
        keys.forEach((k, i) => {
          const r = os.get(k);
          r.onsuccess = () => { out[i] = r.result as T | undefined; };
        });
        tx.oncomplete = () => resolve(out);
        tx.onerror = tx.onabort = () => resolve(keys.map(() => undefined));
      } catch {
        resolve(keys.map(() => undefined));
      }
    });
  }).catch(() => keys.map(() => undefined));
}

async function write(store: string, entries: [string, unknown][]): Promise<void> {
  if (!entries.length) return;
  const db = await open().catch(() => null);
  if (!db) return;
  try {
    const tx = db.transaction(store, 'readwrite');
    const os = tx.objectStore(store);
    for (const [k, v] of entries) os.put(v, k);
    await done(tx);
  } catch { /* quota or closed database */ }
}

async function remove(store: string, keys: string[]): Promise<void> {
  if (!keys.length) return;
  const db = await open().catch(() => null);
  if (!db) return;
  try {
    const tx = db.transaction(store, 'readwrite');
    const os = tx.objectStore(store);
    for (const k of keys) os.delete(k);
    await done(tx);
  } catch { /* closed database */ }
}

/** A threaded list row shows its first message's subject and sender under its latest
 * message's id, so its fields are not that message's own. */
function isThreadRow(row: StoreMessageRow): boolean {
  return row.message_count !== undefined && row.message_count !== null;
}

/** The display fields of a row, or null when the row has none yet (or they are not its own). */
export function metaOf(row: StoreMessageRow): MetaFields | null {
  if (!row || row.meta_pending === true || isThreadRow(row)) return null;
  const out: MetaFields = {};
  for (const f of META_FIELDS) if (row[f] !== undefined) out[f] = row[f];
  return out;
}

export async function getMeta(ids: string[]): Promise<Map<string, MetaFields>> {
  const found = await read<MetaFields>('meta', ids);
  const out = new Map<string, MetaFields>();
  ids.forEach((id, i) => { const m = found[i]; if (m) out.set(id, m); });
  return out;
}

export function putMeta(rows: StoreMessageRow[]): Promise<void> {
  const entries: [string, unknown][] = [];
  for (const r of rows) {
    const m = r?.id ? metaOf(r) : null;
    if (m && r.id) entries.push([String(r.id), m]);
  }
  return write('meta', entries);
}

/** A stable key for a list view. */
export function viewKey(parts: Record<string, unknown>): string {
  return Object.keys(parts).sort().map(k => `${k}=${parts[k] ?? ''}`).join('&');
}

export async function getView(key: string): Promise<CachedView | null> {
  const [v] = await read<CachedView>('views', [key]);
  return v && Array.isArray(v.rows) ? v : null;
}

export function putView(key: string, rows: StoreMessageRow[], total: number, seq: number): Promise<void> {
  const stored = rows.filter(r => r && r.meta_pending !== true).slice(0, VIEW_ROWS);
  return write('views', [[key, { rows: stored, total, seq, at: Date.now() } satisfies CachedView]]);
}

interface StoredBody { value: unknown; at: number; fetched: number }

export async function getBody(key: string): Promise<unknown | undefined> {
  const [b] = await read<StoredBody>('bodies', [key]);
  if (!b) return undefined;
  if (Date.now() - b.fetched > BODY_MAX_AGE_MS) {
    void remove('bodies', [key]);
    return undefined;
  }
  // Touch for LRU (fire and forget).
  void write('bodies', [[key, { ...b, at: Date.now() }]]);
  return b.value;
}

let bodyWrites = 0;

export async function putBody(key: string, value: unknown): Promise<void> {
  const now = Date.now();
  await write('bodies', [[key, { value, at: now, fetched: now } satisfies StoredBody]]);
  // Trim every 50 writes, and whenever the browser reports storage pressure.
  bodyWrites += 1;
  if (bodyWrites % 50 === 0 || await storagePressure()) await trimBodies(BODY_CAP);
}

async function storagePressure(): Promise<boolean> {
  try {
    const est = await navigator.storage?.estimate?.();
    return !!est && !!est.quota && (est.usage || 0) / est.quota > 0.8;
  } catch {
    return false;
  }
}

/** Keep the `cap` most recently read bodies. */
export async function trimBodies(cap: number): Promise<void> {
  const db = await open().catch(() => null);
  if (!db) return;
  try {
    const tx = db.transaction('bodies', 'readwrite');
    const os = tx.objectStore('bodies');
    const countReq = os.count();
    countReq.onsuccess = () => {
      let excess = countReq.result - cap;
      if (excess <= 0) return;
      const cursorReq = os.index('at').openCursor();
      cursorReq.onsuccess = () => {
        const c = cursorReq.result;
        if (!c || excess <= 0) return;
        c.delete();
        excess -= 1;
        c.continue();
      };
    };
    await done(tx);
  } catch { /* closed database */ }
}

/** Drop everything cached for deleted messages. */
export async function forgetMessages(ids: string[]): Promise<void> {
  if (!ids.length) return;
  await remove('meta', ids);
  const db = await open().catch(() => null);
  if (!db) return;
  try {
    const tx = db.transaction('bodies', 'readwrite');
    const os = tx.objectStore('bodies');
    const wanted = new Set(ids);
    const req = os.openKeyCursor();
    req.onsuccess = () => {
      const c = req.result;
      if (!c) return;
      if (wanted.has(String(c.key).split('|')[0])) os.delete(c.key);
      c.continue();
    };
    await done(tx);
  } catch { /* closed database */ }
}

/** Fill display fields of pending rows from the cache; returns the ids still missing. */
export async function fillPendingFromCache(rows: StoreMessageRow[]): Promise<{ rows: StoreMessageRow[]; missing: string[] }> {
  const pending = rows.filter(r => r?.meta_pending === true && r.id && !isThreadRow(r)).map(r => String(r.id));
  if (!pending.length) return { rows, missing: [] };
  const found = await getMeta(pending);
  const missing = pending.filter(id => !found.has(id));
  if (!found.size) return { rows, missing };
  return {
    rows: rows.map(r => {
      const m = r?.id && !isThreadRow(r) ? found.get(String(r.id)) : undefined;
      return m ? ({ ...r, ...m, meta_pending: false } as StoreMessageRow) : r;
    }),
    missing,
  };
}
