// Minimal CalDAV *client* — discovers calendars on a remote server (Nextcloud,
// Fastmail, Radicale, ...) and, for sources the user marked writable, PUTs/DELETEs
// individual events with ETag-based optimistic concurrency. Mirrors
// carddavClient.ts's discovery shape (principal -> home-set -> collections).

import { XMLParser } from 'fast-xml-parser';
import { validateHost } from './hostValidation.js';
import { safeFetch } from './safeFetch.js';
import { toAppError } from '../utils/errors.js';

interface DavRequestOptions {
  username: string;
  password: string;
  depth?: number | string | null;
  body?: unknown;
  allowPrivate?: boolean;
  headers?: Record<string, string>;
}

const parser = new XMLParser({
  ignoreAttributes: false,
  removeNSPrefix: true,
  trimValues: false,
  processEntities: { maxTotalExpansions: 10_000_000, maxExpansionDepth: 10 },
});

/** The credentials + policy one DAV request carries. */
export interface DavCredentials { username: string; password: string; allowPrivate?: boolean }

/** One calendar collection discovered under a CalDAV account's home-set. */
export interface DiscoveredCalendar { href: string; displayName: string; color: string | null }

const toArray = <T>(x: T | T[] | null | undefined): T[] => (Array.isArray(x) ? x : x == null ? [] : [x]);

function basicAuth(username: string, password: string): string {
  return 'Basic ' + Buffer.from(`${username}:${password}`).toString('base64');
}

async function assertHostAllowed(url: string, allowPrivate: boolean): Promise<void> {
  let hostname;
  try { hostname = new URL(url).hostname; }
  catch { throw new Error('Invalid server URL'); }
  const err = await validateHost(hostname, { allowPrivate });
  if (err) throw new Error(err);
}

/** Raw DAV request. Returns the response text plus the status and headers, since
 * write calls need the status code and a returned ETag, not just the body. */
async function dav(method: string, url: string, { username, password, depth, body, allowPrivate = false, headers: extraHeaders }: DavRequestOptions): Promise<{ status: number; text: string; headers: Headers }> {
  // Re-validate on every request: hrefs returned by the server (principal, home
  // set, calendar URLs) are attacker-influenced and could point at internal hosts.
  await assertHostAllowed(url, allowPrivate);
  const headers: Record<string, string> = {
    Authorization: basicAuth(username, password),
    'Content-Type': 'application/xml; charset=utf-8',
    ...extraHeaders,
  };
  if (depth != null) headers.Depth = String(depth);
  let res;
  try {
    // safeFetch validates every redirect hop's IP (well-known discovery relies on
    // the server's 301 redirect), honouring the admin private-host policy.
    res = await safeFetch(url, { method, headers, body, redirect: 'follow', signal: AbortSignal.timeout(30000) }, { allowPrivate });
  } catch (caught) {
    const err = toAppError(caught);
    if (err.name === 'TimeoutError') throw new Error('CalDAV server did not respond (timed out)', { cause: caught });
    throw new Error(`Could not reach the CalDAV server: ${err.message}`, { cause: caught });
  }
  if (res.status === 401) throw new Error('Authentication failed — check the username and app password');
  return { status: res.status, text: await res.text(), headers: res.headers };
}

interface DavPropBlock {
  resourcetype?: Record<string, unknown>;
  displayname?: unknown;
  getetag?: unknown;
  [propName: string]: unknown;
}
type DavPropStat = { status?: unknown; prop?: Record<string, unknown> };

function propsOf(response: { propstat?: DavPropStat | DavPropStat[] }): DavPropBlock {
  const merged: DavPropBlock = {};
  for (const ps of toArray(response.propstat)) {
    const status = typeof ps.status === 'string' ? ps.status : '';
    if (status && !/\b2\d\d\b/.test(status)) continue;
    Object.assign(merged, ps.prop || {});
  }
  return merged;
}

function textOf(node: unknown): string {
  if (node == null) return '';
  if (typeof node === 'string') return node;
  if (typeof node === 'object' && node !== null && '#text' in node) return String((node as Record<string, unknown>)['#text']);
  return '';
}

function absolute(href: string, baseUrl: string): string {
  try { return new URL(href, baseUrl).href; }
  catch { return href; }
}

export function extractHref(xmlText: unknown, key: string, baseUrl: string): string | null {
  const xml = parser.parse(String(xmlText ?? ''));
  const response = toArray(xml?.multistatus?.response)[0];
  if (!response) return null;
  const val: unknown = propsOf(response)[key];
  const href = val !== null && typeof val === 'object' && 'href' in val ? (val as { href?: unknown }).href ?? val : val;
  const text = textOf(href) || (typeof href === 'string' ? href : '');
  return text ? absolute(text, baseUrl) : null;
}

async function propfindHref(url: string, propXml: string, key: string, creds: DavCredentials): Promise<string | null> {
  const body = `<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><prop>${propXml}</prop></propfind>`;
  const { status, text } = await dav('PROPFIND', url, { ...creds, depth: 0, body });
  if (status !== 207) return null;
  return extractHref(text, key, url);
}

// Find the user's principal URL. Tries the given URL, then RFC 6764 well-known
// discovery — most CalDAV servers 301-redirect /.well-known/caldav to the real
// DAV context, and users typically enter just the server's base URL.
async function resolvePrincipal(serverUrl: string, creds: DavCredentials): Promise<string> {
  const origin = new URL(serverUrl).origin;
  const candidates = [serverUrl, `${origin}/.well-known/caldav`];
  let lastErr;
  for (const base of candidates) {
    try {
      const principal = await propfindHref(base, '<current-user-principal/>', 'current-user-principal', creds);
      if (principal) return principal;
    } catch (caught) {
      const err = toAppError(caught);
      if (/Authentication failed/.test(err.message)) throw err; // wrong creds — stop trying
      lastErr = err;
    }
  }
  if (lastErr) throw lastErr;
  return serverUrl; // some servers expose the home set directly at the given URL
}

// Discover every calendar collection on the server for these credentials.
export async function discoverCalendars({ serverUrl, username, password, allowPrivate = false }: { serverUrl: string; username: string; password: string; allowPrivate?: boolean }): Promise<DiscoveredCalendar[]> {
  await assertHostAllowed(serverUrl, allowPrivate);
  const creds = { username, password, allowPrivate };

  const principal = await resolvePrincipal(serverUrl, creds);
  const homeSet = await propfindHref(principal, '<C:calendar-home-set/>', 'calendar-home-set', creds) || principal;

  const body = `<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:IC="http://apple.com/ns/ical/"><prop>
  <resourcetype/><displayname/><IC:calendar-color/></prop></propfind>`;
  const { status, text } = await dav('PROPFIND', homeSet, { ...creds, depth: 1, body });
  if (status !== 207) throw new Error(`CalDAV server rejected calendar discovery (${status})`);
  const calendars = parseCalendars(text, homeSet);
  if (!calendars.length) throw new Error('No calendars found for this account');
  return calendars;
}

// Pure: extract calendar collections from a PROPFIND multistatus. Exported for testing.
export function parseCalendars(xmlText: unknown, baseUrl: string): DiscoveredCalendar[] {
  const xml = parser.parse(String(xmlText ?? ''));
  const calendars: DiscoveredCalendar[] = [];
  for (const response of toArray(xml?.multistatus?.response)) {
    const props = propsOf(response);
    const rt = props.resourcetype || {};
    if (!('calendar' in rt)) continue;
    // Skip special-purpose collections (scheduling inbox/outbox) some servers expose
    // alongside real calendars under the same home-set.
    if ('schedule-inbox' in rt || 'schedule-outbox' in rt) continue;
    const href = textOf(response.href) || response.href;
    if (!href) continue;
    const color = textOf(props['calendar-color']).trim();
    calendars.push({
      href: absolute(href, baseUrl),
      displayName: textOf(props.displayname) || 'Calendar',
      color: /^#[0-9a-fA-F]{6}/.test(color) ? color.slice(0, 7) : null,
    });
  }
  return calendars;
}

/** Result of a write-back attempt. `conflict` means the remote resource changed
 * since we last saw it (ETag mismatch) — the caller should treat `remote` as the
 * authoritative current version rather than retry with the same body. */
export type WriteBackResult =
  | { ok: true; etag: string | null }
  | { ok: false; conflict: true; remote: { ical: string | null; etag: string | null } }
  | { ok: false; conflict: false; error: string };

// PUT one event's ICS. `ifMatchEtag: null` means "create" (If-None-Match: *);
// otherwise it's an update guarded by If-Match against the last-known remote ETag.
export async function putCalendarEvent({ href, ical, ifMatchEtag, username, password, allowPrivate = false }: { href: string; ical: string; ifMatchEtag: string | null; username: string; password: string; allowPrivate?: boolean }): Promise<WriteBackResult> {
  const headers: Record<string, string> = ifMatchEtag === null ? { 'If-None-Match': '*' } : { 'If-Match': `"${ifMatchEtag}"` };
  const { status, headers: resHeaders } = await dav('PUT', href, { username, password, allowPrivate, headers: { ...headers, 'Content-Type': 'text/calendar; charset=utf-8' }, body: ical });
  if (status === 200 || status === 201 || status === 204) {
    const etag = (resHeaders.get('etag') || '').replace(/"/g, '') || null;
    // Some servers (notably ones fronted by a proxy) don't echo an ETag on the PUT
    // response. Fetch it explicitly rather than leaving the local copy without one —
    // an unknown ETag would otherwise make the *next* edit look like a fresh create.
    if (etag) return { ok: true, etag };
    const fetched = await refetchResource(href, { username, password, allowPrivate });
    return { ok: true, etag: fetched.etag };
  }
  if (status === 412 || status === 409) {
    const remote = await refetchResource(href, { username, password, allowPrivate });
    return { ok: false, conflict: true, remote };
  }
  return { ok: false, conflict: false, error: `Remote calendar rejected the write (${status})` };
}

export async function deleteCalendarEvent({ href, ifMatchEtag, username, password, allowPrivate = false }: { href: string; ifMatchEtag: string | null; username: string; password: string; allowPrivate?: boolean }): Promise<WriteBackResult> {
  const { status } = await dav('DELETE', href, { username, password, allowPrivate, headers: ifMatchEtag ? { 'If-Match': `"${ifMatchEtag}"` } : {} });
  if (status === 200 || status === 204 || status === 404) return { ok: true, etag: null };
  if (status === 412 || status === 409) {
    const remote = await refetchResource(href, { username, password, allowPrivate });
    return { ok: false, conflict: true, remote };
  }
  return { ok: false, conflict: false, error: `Remote calendar rejected the delete (${status})` };
}

async function refetchResource(href: string, creds: DavCredentials): Promise<{ ical: string | null; etag: string | null }> {
  try {
    const { status, text, headers } = await dav('GET', href, { ...creds, headers: {} });
    if (status === 404) return { ical: null, etag: null };
    if (status !== 200) return { ical: null, etag: null };
    return { ical: text, etag: (headers.get('etag') || '').replace(/"/g, '') || null };
  } catch {
    // The conflict itself is the fact worth surfacing; a failed refetch just means
    // the caller can't show the newer version yet, not that the write succeeded.
    return { ical: null, etag: null };
  }
}
