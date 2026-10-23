// Optimistic moves: a message moved, archived or deleted shows in its destination folder at
// once. The row is inserted into the destination's cached list views when the move starts
// (so opening that folder renders it immediately) and merged into server responses until the
// server lists it itself; a failed move takes it out again.

import { useStore, type StoreMessageRow } from '../store/index.ts';
import { observeMoves, type MoveTarget } from './api.ts';
import { insertIntoViews, lookupRow, removeFromViews } from './listCache.ts';
import { viewKey } from './mailCache.ts';

/** How long a preview row may wait for the server to list it. */
const PENDING_MS = 60_000;

interface PendingMove {
  row: StoreMessageRow;
  accountId: string;
  folder: string;
  at: number;
}

const pending = new Map<string, PendingMove>();

/** The parameters a list view is keyed by (see MessageList's viewParams). */
export type ViewParams = {
  account: string;
  folder: string;
  unread: boolean;
  threaded: boolean;
  category: string;
};

function destinationFolder(accountId: string, target: MoveTarget): string | null {
  if ('folder' in target) return target.folder;
  const role = target.special;
  const { accounts, folders: allFolders } = useStore.getState();
  const mapped = accounts.find(a => String(a.id) === accountId)?.folder_mappings?.[role];
  if (typeof mapped === 'string' && mapped) return mapped;
  const folders = allFolders[accountId] || [];
  const match = folders.find(f => (f.special_use || '').toLowerCase().includes(role));
  return match?.path ?? null;
}

/** Flat (unthreaded, uncategorized) views a row in `folder` of `accountId` belongs to. */
function viewKeysFor(accountId: string, folder: string, unread: boolean): string[] {
  const accounts = folder === 'INBOX' ? [accountId, ''] : [accountId];
  const keys: string[] = [];
  for (const account of accounts) {
    for (const unreadOnly of unread ? [false, true] : [false]) {
      keys.push(viewKey({ account, folder, unread: unreadOnly, threaded: false, category: '' }));
    }
  }
  return keys;
}

function matches(move: PendingMove, params: ViewParams): boolean {
  if (params.threaded || params.category) return false;
  if (params.unread && move.row.is_read) return false;
  if (params.folder !== move.folder) return false;
  return params.account === move.accountId || (params.account === '' && move.folder === 'INBOX');
}

function expire(now = Date.now()): void {
  for (const [id, move] of pending) if (now - move.at > PENDING_MS) pending.delete(id);
}

/** Start previewing a move; returns the callback that settles it with the server's outcome. */
export function previewMove(ids: string[], target: MoveTarget): ((ok: boolean) => void) | undefined {
  expire();
  const state = useStore.getState();
  const started: PendingMove[] = [];
  for (const id of ids) {
    const source = state.messages.find(m => String(m.id) === id) || lookupRow(id);
    // Thread rows stand for several messages and carry aggregate fields: not previewed.
    if (!source || (source.message_count != null && Number(source.message_count) > 1)) continue;
    const accountId = String(source.account_id || '');
    const folder = accountId ? destinationFolder(accountId, target) : null;
    if (!folder || folder === source.folder) continue;
    const row = { ...source, folder } as StoreMessageRow;
    const move = { row, accountId, folder, at: Date.now() };
    pending.set(id, move);
    started.push(move);
    insertIntoViews(viewKeysFor(accountId, folder, !row.is_read), [row]);
  }
  if (!started.length) return undefined;
  return ok => {
    if (ok) return;
    for (const move of started) {
      const id = String(move.row.id);
      if (pending.get(id) !== move) continue;
      pending.delete(id);
      removeFromViews(viewKeysFor(move.accountId, move.folder, true), [id]);
    }
  };
}

/** Merge preview rows into a server response for a view; rows the server lists stop pending. */
export function withPendingMoves(params: ViewParams, rows: StoreMessageRow[]): StoreMessageRow[] {
  if (!pending.size) return rows;
  expire();
  const present = new Set(rows.map(r => String(r.id)));
  const extra: StoreMessageRow[] = [];
  for (const [id, move] of pending) {
    if (!matches(move, params)) continue;
    if (present.has(id)) pending.delete(id);
    else extra.push(move.row);
  }
  if (!extra.length) return rows;
  const merged = [...extra, ...rows];
  merged.sort((a, b) => {
    const d = Date.parse(String(b.date ?? '')) - Date.parse(String(a.date ?? ''));
    return Number.isNaN(d) ? 0 : d;
  });
  return merged;
}

export function installMovePreview(): void {
  observeMoves(previewMove);
}
