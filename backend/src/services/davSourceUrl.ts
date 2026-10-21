// Shared URL/collection validation for DAV (CalDAV + CardDAV) remote sources.
// Used by both routes/calendar.ts (kind='ical_url' subscriptions + legacy direct
// calendar-source edits) and routes/davServers.ts (the unified CalDAV/CardDAV
// server manager) so the SSRF-sensitive host validation lives in exactly one place.

import { validateHost } from './hostValidation.js';
import { getConnectionPolicy } from './connectionPolicy.js';
import { hrefHash } from '../utils/hrefHash.js';

/** Validate a URL string: http(s) only, no embedded credentials, and SSRF-checked
 * against the connection policy. The policy is only fetched once the cheap
 * synchronous checks pass, so a malformed URL or one carrying credentials never
 * touches the database. */
export async function validateSourceUrl(raw: unknown): Promise<{ error: string } | { url: URL; policy: { allowPrivateHosts?: boolean } }> {
  let parsed;
  try { parsed = new URL(String(raw)); } catch { return { error: 'Invalid source URL' }; }
  if (parsed.protocol === 'webcal:') parsed = new URL(String(raw).replace(/^[^:]+:/, 'https:'));
  if (parsed.username || parsed.password) return { error: 'Source URL must not include credentials' };
  if (!['https:', 'http:'].includes(parsed.protocol)) return { error: 'Source URL must use http(s)' };
  const policy = await getConnectionPolicy();
  const hostError = await validateHost(parsed.hostname, { allowPrivate: policy.allowPrivateHosts });
  if (hostError) return { error: hostError };
  if (parsed.protocol === 'http:') {
    if (!policy.allowPrivateHosts) return { error: 'Source URL must use HTTPS' };
    const publicHostError = await validateHost(parsed.hostname, { allowPrivate: false });
    if (!publicHostError) return { error: 'HTTPS is required for a public source' };
  }
  return { url: parsed, policy };
}

/** A CalDAV/CardDAV server URL is optional: many providers (Fastmail, iCloud, some
 * Nextcloud setups) can be found from just the account's email address via RFC 6764
 * well-known discovery against the address's domain. When `rawUrl` is blank,
 * `username` must be that email address, and `https://<domain>/` becomes the URL
 * discovery/storage use — identical to the user having typed that URL directly. */
export function resolveCaldavUrl(rawUrl: unknown, username: unknown): { url: string } | { error: string } {
  if (rawUrl) return { url: String(rawUrl) };
  const match = typeof username === 'string' ? username.trim().match(/^[^\s@]+@([^\s@]+\.[^\s@]+)$/) : null;
  if (!match) return { error: 'Enter a server URL, or leave it blank and use your email address as the username' };
  return { url: `https://${match[1].toLowerCase()}/` };
}

/** A source's sync interval in seconds (15–86400); defaults to the smallest, 15 s. */
export function clampIntervalSeconds(value: unknown, fallback = 15): number {
  return Math.max(15, Math.min(86400, Number.parseInt(String(value), 10) || fallback));
}

/** Incoming collection entries from the client: either a full replace (has `href`,
 * from a fresh discovery run) or a selection-only toggle (`id` = the opaque href hash). */
export type IncomingCollection = { href?: string; id?: string; displayName?: string; color?: string | null; selected?: boolean };

/** Validate one discovered collection's href the same way the top-level source URL
 * is validated. Every href is attacker-influenced client JSON (it round-trips through
 * the client between discovery and create/update), so it gets its own SSRF check
 * before it's ever stored or fetched — independent of the top-level URL's check. */
export async function validatedCollectionHref(href: unknown, policy: { allowPrivateHosts?: boolean }): Promise<{ href: string; hrefHash: string } | { error: string }> {
  if (!href || typeof href !== 'string') return { error: 'Each item to import needs an href' };
  let collectionUrl: URL;
  try { collectionUrl = new URL(href); } catch { return { error: 'Invalid collection href' }; }
  const hostError = await validateHost(collectionUrl.hostname, { allowPrivate: policy.allowPrivateHosts });
  if (hostError) return { error: hostError };
  const normalized = collectionUrl.toString();
  return { href: normalized, hrefHash: hrefHash(normalized) };
}
