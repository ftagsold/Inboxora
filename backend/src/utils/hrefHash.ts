import crypto from 'crypto';

/** Opaque, deterministic identifier for a discovered DAV collection href. Sent to
 * the client instead of the href itself (never the href — same secrecy as `url`)
 * and used to link a local `calendars`/`address_books` row back to its remote
 * collection via `external_url = 'source:<sourceId>:<hrefHash>'`. */
export function hrefHash(href: string): string {
  return crypto.createHash('sha256').update(href).digest('hex').slice(0, 32);
}
