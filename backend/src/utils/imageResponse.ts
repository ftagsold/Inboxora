import type { Response } from 'express';

/** Raster types a browser renders inertly. SVG and anything else can carry script. */
const SAFE_IMAGE_TYPES = new Set(['image/jpeg', 'image/png', 'image/gif', 'image/webp']);

/** The safe raster MIME type for `type` (parameters ignored), or null when it is not one. */
export function safeImageType(type: string | null | undefined): string | null {
  const base = String(type ?? '').split(';')[0].trim().toLowerCase();
  if (base === 'image/jpg') return 'image/jpeg';
  return SAFE_IMAGE_TYPES.has(base) ? base : null;
}

/**
 * Send third-party image bytes (contact photos, proxied avatars) from the app's own origin.
 * Only raster types are served, and the response is sandboxed so that opening its URL
 * directly can never run script in the session.
 */
export function sendImage(res: Response, type: string | null | undefined, body: Buffer) {
  const safeType = safeImageType(type);
  if (!safeType) return res.status(404).end();
  res.set({
    'Content-Type': safeType,
    'Content-Security-Policy': "default-src 'none'; sandbox",
    'X-Content-Type-Options': 'nosniff',
  });
  return res.send(body);
}
