// Background prefetch of list views and bodies, so the next click is served from the cache:
// every account's INBOX once the app is idle, a folder's first page on sidebar hover, and the
// bodies of the first unread rows of a loaded list. At most two requests run at a time and a
// view fetched in the last minute is not fetched again.

import { useStore } from '../store/index.ts';
import type { StoreMessageRow } from '../store/index.ts';
import { api, type QueryParams } from './api.ts';
import { rememberView } from './listCache.ts';
import { viewKey } from './mailCache.ts';

const CONCURRENCY = 2;
const FRESH_MS = 60_000;
/** Unread bodies prefetched per list load. */
export const UNREAD_BODY_PREFETCH = 8;

const queue: Array<() => Promise<unknown>> = [];
let running = 0;
const fetchedAt = new Map<string, number>();

function pump(): void {
  while (running < CONCURRENCY && queue.length) {
    const job = queue.shift();
    if (!job) break;
    running += 1;
    void job().catch(() => {}).finally(() => { running -= 1; pump(); });
  }
}

function enqueue(job: () => Promise<unknown>): void {
  queue.push(job);
  const idle = (globalThis as { requestIdleCallback?: (cb: () => void, opts?: { timeout: number }) => void }).requestIdleCallback;
  if (idle) idle(pump, { timeout: 2000 });
  else setTimeout(pump, 200);
}

/** The cache key and request parameters of a folder's first page, as MessageList builds them. */
export function viewRequest(accountId: string | null, folder: string): { key: string; params: QueryParams } {
  const state = useStore.getState();
  const account = state.accounts.find(a => a.id === accountId);
  const categorized = folder === 'INBOX' && (state.categorizationEnabled || !!account?.categorization_enabled);
  const parts = {
    account: accountId || '',
    folder: accountId ? folder : 'INBOX',
    unread: false,
    threaded: state.threadedView,
    category: categorized ? 'primary' : '',
  };
  const params: QueryParams = { limit: state.pageSize, offset: 0 };
  if (accountId) { params.accountId = accountId; params.folder = folder; }
  if (state.threadedView) params.threaded = 'true';
  if (categorized) params.category = 'primary';
  return { key: viewKey(parts), params };
}

/** Fetch a folder's first page into the view cache (skipped when fetched recently). */
export function prefetchView(accountId: string | null, folder: string): void {
  const { key, params } = viewRequest(accountId, folder);
  const last = fetchedAt.get(key) || 0;
  if (Date.now() - last < FRESH_MS) return;
  fetchedAt.set(key, Date.now());
  enqueue(async () => {
    const data: { messages: StoreMessageRow[]; total: number; seq?: number } = await api.getMessages(params);
    rememberView(key, data.messages, data.total, data.seq || 0);
  });
}

/** Every account's INBOX (and the unified inbox) once the browser is idle. */
export function prefetchInboxes(): void {
  const accounts = useStore.getState().accounts.filter(a => a.enabled !== false && a.protocol !== 'jmap');
  prefetchView(null, 'INBOX');
  for (const a of accounts) if (a.id) prefetchView(String(a.id), 'INBOX');
}

/** Bodies of the first unread rows (cached in the browser by api.getMessageBody). */
export function prefetchUnreadBodies(rows: StoreMessageRow[]): void {
  const unread = rows.filter(r => r && r.is_read === false && r.meta_pending !== true && r.message_count == null)
    .slice(0, UNREAD_BODY_PREFETCH);
  for (const r of unread) {
    const key = `body:${r.id}`;
    if (fetchedAt.has(key)) continue;
    fetchedAt.set(key, Date.now());
    enqueue(() => api.getMessageBody(String(r.id)));
  }
}
