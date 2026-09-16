import { describe, it, expect, beforeEach, vi } from 'vitest';

vi.mock('./db.js', () => ({ query: vi.fn() }));
vi.mock('./encryption.js', () => ({ encrypt: (v: string) => `enc:${v}` }));
vi.mock('./hostValidation.js', () => ({ validateHost: vi.fn(async () => null) }));
vi.mock('./oidcEnvConfig.js', () => ({ getOidcEnvProviders: vi.fn() }));

const { query } = vi.mocked(await import('./db.js'));
const { validateHost } = vi.mocked(await import('./hostValidation.js'));
const { getOidcEnvProviders } = vi.mocked(await import('./oidcEnvConfig.js'));
const { seedOidcProvidersFromEnv } = await import('./oidcEnvSeed.js');

const envProvider = {
  index: 1, name: 'Corp SSO', slug: 'corp-sso',
  issuerUrl: 'https://idp.example.com', clientId: 'cid', clientSecret: 'csecret',
  scopes: 'openid email profile', provisioningMode: 'login_existing_only',
  allowedDomains: null, enabled: true, requireEmailVerified: true, allowInsecure: false,
  adminGroupClaim: null, adminGroupValue: null, rpInitiatedLogout: false, loginMatchClaim: 'email',
  mail: null,
};

describe('seedOidcProvidersFromEnv', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('does nothing when there are no env-configured providers', async () => {
    getOidcEnvProviders.mockReturnValue([]);
    await seedOidcProvidersFromEnv();
    expect(query).not.toHaveBeenCalled();
  });

  it('inserts a provider that does not exist yet, with the client secret encrypted', async () => {
    getOidcEnvProviders.mockReturnValue([envProvider]);
    query
      .mockResolvedValueOnce({ rows: [] }) // SELECT by slug
      .mockResolvedValueOnce({ rows: [] }); // INSERT

    await seedOidcProvidersFromEnv();

    expect(query).toHaveBeenCalledTimes(2);
    expect(query.mock.calls[0][0]).toMatch(/SELECT id FROM oidc_providers WHERE slug/);
    const [insertSql, insertParams] = query.mock.calls[1];
    expect(insertSql).toMatch(/INSERT INTO oidc_providers/);
    expect(insertParams).toEqual([
      'Corp SSO', 'corp-sso', 'https://idp.example.com', 'cid', 'enc:csecret',
      'openid email profile', 'login_existing_only', null, true, true,
      false, null, null, false, 'email',
    ]);
  });

  it('does not touch a provider that already exists (bootstrap-once)', async () => {
    getOidcEnvProviders.mockReturnValue([envProvider]);
    query.mockResolvedValueOnce({ rows: [{ id: 'existing-id' }] }); // SELECT finds a row

    await seedOidcProvidersFromEnv();

    expect(query).toHaveBeenCalledTimes(1);
  });

  it('skips a provider whose issuer host is rejected and does not insert it', async () => {
    getOidcEnvProviders.mockReturnValue([envProvider]);
    validateHost.mockResolvedValueOnce('private/loopback address');

    await seedOidcProvidersFromEnv();

    expect(query).not.toHaveBeenCalled();
  });

  it('skips host validation entirely when allow_insecure is set', async () => {
    getOidcEnvProviders.mockReturnValue([{ ...envProvider, allowInsecure: true }]);
    query.mockResolvedValueOnce({ rows: [] }).mockResolvedValueOnce({ rows: [] });

    await seedOidcProvidersFromEnv();

    expect(validateHost).not.toHaveBeenCalled();
    expect(query).toHaveBeenCalledTimes(2);
  });

  it('continues seeding remaining providers when one insert fails', async () => {
    const other = { ...envProvider, index: 2, slug: 'other-sso', name: 'Other SSO' };
    getOidcEnvProviders.mockReturnValue([envProvider, other]);
    query
      .mockResolvedValueOnce({ rows: [] })
      .mockRejectedValueOnce(new Error('db exploded'))
      .mockResolvedValueOnce({ rows: [] })
      .mockResolvedValueOnce({ rows: [] });

    await expect(seedOidcProvidersFromEnv()).resolves.toBeUndefined();
    expect(query).toHaveBeenCalledTimes(4);
  });
});
