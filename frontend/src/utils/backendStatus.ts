// Tracks whether the Inboxora backend is reachable. A dropped WebSocket or a failed /api
// call only raises suspicion; the backend counts as down once a /api/health probe fails
// too, and as up again as soon as a probe (or any request) succeeds. The UI blocks with a
// reconnecting screen while it is down.

import { useSyncExternalStore } from 'react';

const HEALTH_URL = '/api/health';
const PROBE_TIMEOUT_MS = 4000;
const RETRY_MS = 2500;

let down = false;
let probing = false;
let retryTimer: ReturnType<typeof setTimeout> | undefined = undefined;
const listeners = new Set<() => void>();

function setDown(next: boolean): void {
  if (down === next) return;
  down = next;
  // Let the WebSocket skip its backoff and reconnect right away.
  if (!next && typeof window !== 'undefined') window.dispatchEvent(new CustomEvent('inboxora:backend_up'));
  for (const listener of listeners) listener();
}

async function probe(): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
  try {
    const res = await fetch(HEALTH_URL, { cache: 'no-store', credentials: 'include', signal: controller.signal });
    return res.ok;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

async function check(): Promise<void> {
  if (probing) return;
  probing = true;
  clearTimeout(retryTimer);
  const ok = await probe();
  probing = false;
  setDown(!ok);
  if (!ok) retryTimer = setTimeout(() => { void check(); }, RETRY_MS);
}

/** A request or the WebSocket failed in a way that may mean the backend is gone. */
export function reportBackendFailure(): void {
  // Browser only: without a page (unit tests) the retry loop would keep the process alive.
  if (typeof window === 'undefined' || typeof fetch === 'undefined') return;
  void check();
}

/** The backend answered: clear the down state without waiting for the next probe. */
export function reportBackendOk(): void {
  if (!down) return;
  clearTimeout(retryTimer);
  setDown(false);
}

/** Gateway answers that mean the backend behind the proxy is unreachable. */
export function isGatewayFailure(status: number): boolean {
  return status === 502 || status === 503 || status === 504;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function useBackendDown(): boolean {
  return useSyncExternalStore(subscribe, () => down, () => false);
}
