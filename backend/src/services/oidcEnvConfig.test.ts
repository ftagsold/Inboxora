import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { getOidcEnvProviders, getMailConfigForSlug, reloadOidcEnvProviders } from './oidcEnvConfig.js';

const OIDC_PREFIX = /^OIDC_\d+_/;

function clearOidcEnv() {
  for (const key of Object.keys(process.env)) {
    if (OIDC_PREFIX.test(key)) delete process.env[key];
  }
}

function reload() {
  reloadOidcEnvProviders();
  return getOidcEnvProviders();
}

describe('oidcEnvConfig', () => {
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    clearOidcEnv();
    errorSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
  });

  afterEach(() => {
    clearOidcEnv();
    reloadOidcEnvProviders();
    errorSpy.mockRestore();
  });

  it('returns no providers when no OIDC_<N>_* env vars are set', () => {
    expect(reload()).toEqual([]);
  });

  it('parses a minimal provider with defaults', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';

    const [p] = reload();
    expect(p).toMatchObject({
      index: 1,
      name: 'SSO 1',
      slug: 'sso-1',
      issuerUrl: 'https://idp.example.com',
      clientId: 'client-1',
      clientSecret: 'secret-1',
      scopes: 'openid email profile',
      provisioningMode: 'login_existing_only',
      enabled: true,
      requireEmailVerified: true,
      allowInsecure: false,
      loginMatchClaim: 'email',
      mail: null,
    });
  });

  it('skips a provider missing required fields and logs why', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    // no CLIENT_SECRET

    expect(reload()).toEqual([]);
    expect(errorSpy).toHaveBeenCalledWith(expect.stringContaining('OIDC_1_CLIENT_SECRET is required'));
  });

  it('rejects a non-HTTPS issuer unless ALLOW_INSECURE is set', () => {
    process.env.OIDC_1_ISSUER_URL = 'http://idp.internal';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    expect(reload()).toEqual([]);

    process.env.OIDC_1_ALLOW_INSECURE = 'true';
    const [p] = reload();
    expect(p.issuerUrl).toBe('http://idp.internal');
    expect(p.allowInsecure).toBe(true);
  });

  it('parses multiple indexed providers independently', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp1.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_2_ISSUER_URL = 'https://idp2.example.com';
    process.env.OIDC_2_CLIENT_ID = 'client-2';
    process.env.OIDC_2_CLIENT_SECRET = 'secret-2';
    process.env.OIDC_2_SLUG = 'corp-sso';
    process.env.OIDC_2_NAME = 'Corp SSO';

    const providers = reload();
    expect(providers).toHaveLength(2);
    expect(providers[0].slug).toBe('sso-1');
    expect(providers[1]).toMatchObject({ slug: 'corp-sso', name: 'Corp SSO' });
  });

  it('warns and skips both on a slug collision between two indices', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp1.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_SLUG = 'dup';
    process.env.OIDC_2_ISSUER_URL = 'https://idp2.example.com';
    process.env.OIDC_2_CLIENT_ID = 'client-2';
    process.env.OIDC_2_CLIENT_SECRET = 'secret-2';
    process.env.OIDC_2_SLUG = 'dup';

    const providers = reload();
    expect(providers).toHaveLength(2);
    expect(errorSpy).toHaveBeenCalledWith(expect.stringContaining('both resolve to slug "dup"'));
  });

  it('rejects an invalid login_match_claim', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_LOGIN_MATCH_CLAIM = '__proto__';
    expect(reload()).toEqual([]);
  });

  it('activates mail passthrough only when both IMAP and SMTP hosts are set, defaulting the username claim to email', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_MAIL_IMAP_HOST = 'imap.example.com';
    process.env.OIDC_1_MAIL_SMTP_HOST = 'smtp.example.com';

    const [p] = reload();
    expect(p.mail).toMatchObject({
      imapHost: 'imap.example.com',
      imapPort: 993,
      imapTls: true,
      smtpHost: 'smtp.example.com',
      smtpPort: 587,
      smtpTls: 'STARTTLS',
      usernameClaim: 'email',
      skipTlsVerify: false,
    });
    expect(getMailConfigForSlug('sso-1')).toEqual(p.mail);
  });

  it('honors a custom mail username claim', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_MAIL_IMAP_HOST = 'imap.example.com';
    process.env.OIDC_1_MAIL_SMTP_HOST = 'smtp.example.com';
    process.env.OIDC_1_MAIL_USERNAME_CLAIM = 'preferred_username';

    const [p] = reload();
    expect(p.mail!.usernameClaim).toBe('preferred_username');
  });

  it('defaults the mailbox display name to null, honoring MAIL_NAME when set', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_MAIL_IMAP_HOST = 'imap.example.com';
    process.env.OIDC_1_MAIL_SMTP_HOST = 'smtp.example.com';

    expect(reload()[0].mail!.displayName).toBeNull();

    process.env.OIDC_1_MAIL_NAME = 'Company Mail';
    expect(reload()[0].mail!.displayName).toBe('Company Mail');
  });

  it('disables mail passthrough (but keeps the provider, e.g. for login) when only one mail host is set', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_MAIL_IMAP_HOST = 'imap.example.com';
    // no SMTP host

    const [p] = reload();
    expect(p).toBeTruthy();
    expect(p.mail).toBeNull();
    expect(getMailConfigForSlug('sso-1')).toBeNull();
    expect(errorSpy).toHaveBeenCalledWith(expect.stringContaining('SSO login is unaffected'));
  });

  it('rejects an invalid smtp_tls value for mail passthrough without disabling login', () => {
    process.env.OIDC_1_ISSUER_URL = 'https://idp.example.com';
    process.env.OIDC_1_CLIENT_ID = 'client-1';
    process.env.OIDC_1_CLIENT_SECRET = 'secret-1';
    process.env.OIDC_1_MAIL_IMAP_HOST = 'imap.example.com';
    process.env.OIDC_1_MAIL_SMTP_HOST = 'smtp.example.com';
    process.env.OIDC_1_MAIL_SMTP_TLS = 'bogus';

    const [p] = reload();
    expect(p).toBeTruthy();
    expect(p.mail).toBeNull();
  });

  it('returns null from getMailConfigForSlug for an unknown slug', () => {
    reload();
    expect(getMailConfigForSlug('does-not-exist')).toBeNull();
  });
});
