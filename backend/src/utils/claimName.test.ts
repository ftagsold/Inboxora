import { describe, it, expect } from 'vitest';
import { normalizeClaimName, resolveClaimValue } from './claimName.js';

describe('normalizeClaimName', () => {
  it('accepts a well-formed claim name and trims it', () => {
    expect(normalizeClaimName('  preferred_username ')).toBe('preferred_username');
    expect(normalizeClaimName('upn')).toBe('upn');
    expect(normalizeClaimName('a.b:c-d_1')).toBe('a.b:c-d_1');
  });

  it('rejects an empty or overlong value', () => {
    expect(normalizeClaimName('')).toBeNull();
    expect(normalizeClaimName('   ')).toBeNull();
    expect(normalizeClaimName('a'.repeat(65))).toBeNull();
  });

  it('rejects characters outside the safe charset', () => {
    expect(normalizeClaimName('foo bar')).toBeNull();
    expect(normalizeClaimName('foo/bar')).toBeNull();
    expect(normalizeClaimName('foo"bar')).toBeNull();
  });

  it('rejects object-prototype key names', () => {
    expect(normalizeClaimName('__proto__')).toBeNull();
    expect(normalizeClaimName('constructor')).toBeNull();
    expect(normalizeClaimName('prototype')).toBeNull();
  });
});

describe('resolveClaimValue', () => {
  const email = 'Alice@Example.com';
  const payload = { email, preferred_username: 'Alice', upn: 'alice@corp.local' };

  it('defaults to the verified email when claim is unset', () => {
    expect(resolveClaimValue(undefined, payload, email)).toBe('alice@example.com');
    expect(resolveClaimValue('', payload, email)).toBe('alice@example.com');
  });

  it('reads a configured claim from the payload, lowercased and trimmed', () => {
    expect(resolveClaimValue('preferred_username', payload, email)).toBe('alice');
    expect(resolveClaimValue('upn', payload, email)).toBe('alice@corp.local');
  });

  it('returns null for a missing, non-string, or blank claim', () => {
    expect(resolveClaimValue('missing', payload, email)).toBeNull();
    expect(resolveClaimValue('preferred_username', { preferred_username: 42 }, email)).toBeNull();
    expect(resolveClaimValue('preferred_username', { preferred_username: '   ' }, email)).toBeNull();
  });

  it('never indexes the payload by an object-prototype key', () => {
    expect(resolveClaimValue('__proto__', payload, email)).toBeNull();
  });
});
