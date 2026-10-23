// Message list view cache: the rows of recently opened views kept in memory (and mirrored to
// IndexedDB through mailCache.ts) so switching back to a folder renders at once, plus the
// helpers that revalidate a view: merging a change feed into the loaded rows, the keyset
// cursor of the next page, and filling rows the server served without display fields.

import type { StoreMessageRow } from '../store/index.ts';
import { META_FIELDS, fillPendingFromCache, getView, putMeta, putView, type CachedView } from './mailCache.ts';

/** Views kept in memory. */
export const MEMORY_VIEWS = 12;
/** Ids per POST /mail/meta request. */
export const META_BATCH = 100;

const memory = new Map<string, CachedView>();

export function rememberView(key: string, rows: StoreMessageRow[], total: number, seq: number): void {
  memory.delete(key);
  memory.set(key, { rows, total, seq, at: Date.now() });
  while (memory.size > MEMORY_VIEWS) {
    const oldest = memory.keys().next().value;
    if (oldest === undefined) break;
    memory.delete(oldest);
  }
  void putView(key, rows, total, seq);
  void putMeta(rows);
}

/** The cached rows of a view: memory first, then IndexedDB. */
export async function recallView(key: string): Promise<CachedView | null> {
  const hit = memory.get(key);
  if (hit) return hit;
  const stored = await getView(key);
  if (stored) memory.set(key, stored);
  return stored;
}

export function clearMemoryViews(): void {
  memory.clear();
}

function dateMs(value: unknown): number {
  if (value instanceof Date) return value.getTime();
  if (typeof value === 'number') return value;
  if (typeof value === 'string') {
    const t = Date.parse(value);
    return Number.isNaN(t) ? 0 : t;
  }
  return 0;
}

/** Newest first, ties by id (the server's ORDER BY date DESC, id DESC). */
export function compareRows(a: StoreMessageRow, b: StoreMessageRow): number {
  const d = dateMs(b.date) - dateMs(a.date);
  if (d !== 0) return d;
  const ai = String(a.id);
  const bi = String(b.id);
  return ai === bi ? 0 : ai < bi ? 1 : -1;
}

/** The keyset cursor after the last loaded row (`<date>|<id>`), or null. */
export function cursorOf(rows: StoreMessageRow[]): string | null {
  const last = rows[rows.length - 1];
  if (!last?.id || last.date == null) return null;
  const ms = dateMs(last.date);
  if (!ms) return null;
  return `${new Date(ms).toISOString()}|${last.id}`;
}

/** A changed row without display fields must not blank the ones the loaded row has. */
function withoutPendingMeta(row: StoreMessageRow): Partial<StoreMessageRow> {
  if (row.meta_pending !== true) return row;
  const copy: Record<string, unknown> = { ...row };
  for (const f of META_FIELDS) delete copy[f];
  delete copy.meta_pending;
  return copy as Partial<StoreMessageRow>;
}

/**
 * Apply a change feed to the loaded rows of a view: deleted and no-longer-matching rows drop
 * out, changed rows are updated in place, and new rows newer than the oldest loaded one are
 * inserted in date order (older ones belong to pages not loaded yet).
 */
export function mergeChanges(
  rows: StoreMessageRow[],
  changed: StoreMessageRow[],
  removed: string[],
): StoreMessageRow[] {
  const gone = new Set(removed.map(String));
  const updates = new Map(changed.map(r => [String(r.id), r]));
  const kept = rows
    .filter(r => !gone.has(String(r.id)))
    .map(r => {
      const u = updates.get(String(r.id));
      return u ? ({ ...r, ...withoutPendingMeta(u) } as StoreMessageRow) : r;
    });
  const present = new Set(kept.map(r => String(r.id)));
  const oldest = kept.length ? dateMs(kept[kept.length - 1].date) : Number.NEGATIVE_INFINITY;
  const fresh = changed.filter(r => !present.has(String(r.id)) && !gone.has(String(r.id)) && dateMs(r.date) >= oldest);
  if (!fresh.length) return kept;
  return [...fresh, ...kept].sort(compareRows);
}

/**
 * Fill rows served with `meta_pending`: from the browser cache first, then from the server in
 * batches. `apply` receives each batch of filled rows by id.
 */
export async function hydratePendingRows(
  rows: StoreMessageRow[],
  fetchMeta: (ids: string[]) => Promise<{ messages: StoreMessageRow[] }>,
  apply: (patches: Map<string, Partial<StoreMessageRow>>) => void,
): Promise<void> {
  const { rows: filled, missing } = await fillPendingFromCache(rows);
  const fromCache = new Map<string, Partial<StoreMessageRow>>();
  filled.forEach((r, i) => {
    if (rows[i]?.meta_pending === true && r.meta_pending !== true) fromCache.set(String(r.id), r);
  });
  if (fromCache.size) apply(fromCache);
  for (let i = 0; i < missing.length; i += META_BATCH) {
    const ids = missing.slice(i, i + META_BATCH);
    try {
      const data = await fetchMeta(ids);
      const patches = new Map<string, Partial<StoreMessageRow>>();
      for (const m of data.messages || []) {
        if (m?.id && m.meta_pending !== true) patches.set(String(m.id), m);
      }
      if (patches.size) {
        apply(patches);
        void putMeta([...patches.values()] as StoreMessageRow[]);
      }
    } catch {
      // The rows keep their placeholders; the next refresh asks again.
    }
  }
}

/** Patch rows by id (display fields only, never flags the user may have just changed). */
export function patchRows(rows: StoreMessageRow[], patches: Map<string, Partial<StoreMessageRow>>): StoreMessageRow[] {
  if (!patches.size) return rows;
  return rows.map(r => {
    const p = patches.get(String(r.id));
    if (!p) return r;
    const meta: Record<string, unknown> = {};
    for (const f of META_FIELDS) if (p[f] !== undefined) meta[f] = p[f];
    return { ...r, ...meta, meta_pending: false } as StoreMessageRow;
  });
}

/** Give pending rows the display fields the rows already on screen have for the same id, so a
 * revalidation never blanks a row the user can see. */
export function carryMeta(fresh: StoreMessageRow[], previous: StoreMessageRow[]): StoreMessageRow[] {
  if (!previous.length || !fresh.some(r => r?.meta_pending === true)) return fresh;
  const known = new Map(previous.filter(r => r && r.meta_pending !== true).map(r => [String(r.id), r]));
  return fresh.map(r => {
    if (r?.meta_pending !== true) return r;
    const k = known.get(String(r.id));
    if (!k || k.message_count != null || r.message_count != null) return r;
    const meta: Record<string, unknown> = {};
    for (const f of META_FIELDS) if (k[f] !== undefined) meta[f] = k[f];
    return { ...r, ...meta, meta_pending: false } as StoreMessageRow;
  });
}
