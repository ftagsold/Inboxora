// Retired addresses: old inboxes the user still forwards from but wants to move away from.
// A mail that reached one of them (To, Cc, or the delivery headers the forwarder kept) gets a
// small tag, so the user can update their address at that sender, one by one.
import { parseAddressListField } from './replyAlias.ts';

export const RETIRED_ADDRESSES_MAX = 50;

const EMAIL = /^[^\s@<>]+@[^\s@<>]+\.[^\s@<>]+$/;

/** Trimmed, lowercase, de-duplicated valid addresses, at most RETIRED_ADDRESSES_MAX. */
export function normalizeRetiredAddresses(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const item of value) {
    const email = typeof item === 'string' ? item.trim().toLowerCase() : '';
    if (EMAIL.test(email) && !out.includes(email)) out.push(email);
    if (out.length >= RETIRED_ADDRESSES_MAX) break;
  }
  return out;
}

function emailsOf(field: unknown): string[] {
  return parseAddressListField<unknown>(field).map(entry => {
    const raw = typeof entry === 'string'
      ? entry
      : (entry && typeof entry === 'object' ? ((entry as { email?: unknown; address?: unknown }).email ?? (entry as { address?: unknown }).address) : '');
    const s = String(raw ?? '').trim();
    const angle = s.match(/<([^>]+)>/);
    return (angle ? angle[1] : s).trim().toLowerCase();
  }).filter(Boolean);
}

/** The first retired address this mail was sent or delivered to, or null. */
export function retiredAddressOf(retired: readonly string[], ...fields: unknown[]): string | null {
  if (!retired.length) return null;
  for (const field of fields) {
    for (const email of emailsOf(field)) {
      if (retired.includes(email)) return email;
    }
  }
  return null;
}
