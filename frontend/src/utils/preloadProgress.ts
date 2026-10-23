// Progress of the server's cold cache preload, as the blocking preload screen shows it.

import type { PreloadAccountStatus, PreloadStatus } from './api.ts';

export interface PreloadEventDetail {
  accountId?: string;
  state?: PreloadAccountStatus['state'];
  done?: number;
  total?: number;
  messages?: number;
  folder?: string | null;
  error?: string | null;
}

/** Fold one progress event into the status (unknown accounts are left to the next poll). */
export function applyPreloadEvent(status: PreloadStatus, ev: PreloadEventDetail): PreloadStatus {
  if (!ev.accountId) return status;
  let known = false;
  const accounts = status.accounts.map(a => {
    if (a.id !== ev.accountId) return a;
    known = true;
    return {
      ...a,
      state: ev.state ?? a.state,
      done: typeof ev.done === 'number' ? ev.done : a.done,
      total: typeof ev.total === 'number' ? ev.total : a.total,
      messages: typeof ev.messages === 'number' ? ev.messages : a.messages,
      folder: ev.folder !== undefined ? ev.folder : a.folder,
      error: ev.error !== undefined ? ev.error : a.error,
    };
  });
  if (!known) return status;
  return { accounts, ready: accounts.every(a => a.state === 'done' || a.state === 'failed') };
}

/** Overall progress 0..1, or null while no account knows its size yet. */
export function overallProgress(accounts: PreloadAccountStatus[]): number | null {
  const total = accounts.reduce((n, a) => n + (a.state === 'failed' ? 0 : a.total), 0);
  if (!total) return accounts.length && accounts.every(a => a.state === 'done' || a.state === 'failed') ? 1 : null;
  const done = accounts.reduce((n, a) => n + (a.state === 'failed' ? 0 : a.state === 'done' ? a.total : Math.min(a.done, a.total)), 0);
  return Math.max(0, Math.min(1, done / total));
}
