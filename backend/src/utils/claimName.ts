// Shared handling for "claim name" config values — the OIDC claim (from a verified id_token)
// an admin or deployment operator points at a specific piece of data (which claim identifies
// the user, which claim names their IMAP/SMTP mailbox login, etc). Centralized so every call
// site enforces the same charset and blocks the same object-prototype-key footgun.

const DANGEROUS_CLAIM_KEYS = new Set(['__proto__', 'constructor', 'prototype']);

// Validates a claim name against a safe charset and rejects object-prototype key names.
// Returns the trimmed claim name, or null if it is not a valid claim name.
export function normalizeClaimName(v: unknown): string | null {
  const c = String(v ?? '').trim();
  if (!/^[a-zA-Z0-9_.:-]{1,64}$/.test(c)) return null;
  if (DANGEROUS_CLAIM_KEYS.has(c)) return null;
  return c;
}

// Resolves the value of `claim` from a verified id_token payload. The claim name 'email'
// (the default) is special-cased to the caller-supplied, already-verified `email` value rather
// than reading payload.email again, so callers can't be tricked by a payload that has both an
// 'email' claim and some other claim disagreeing with it. `payload["__proto__"]` from
// JSON.parse is a string own-property that would pass a naive typeof guard — DANGEROUS_CLAIM_KEYS
// blocks it explicitly. Returns a lowercased, trimmed string, or null if the claim is
// absent/blank/non-string.
export function resolveClaimValue(
  claim: string | null | undefined,
  payload: Record<string, unknown> | null | undefined,
  email: string | null | undefined
): string | null {
  const c = (claim || 'email').trim() || 'email';
  if (c !== 'email' && DANGEROUS_CLAIM_KEYS.has(c)) return null;
  const raw = c === 'email' ? email : payload?.[c];
  return typeof raw === 'string' && raw.trim() ? raw.trim().toLowerCase() : null;
}
