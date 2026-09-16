import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';

vi.mock('./db.js', () => ({ query: vi.fn() }));
vi.mock('./encryption.js', () => ({
  encrypt: (v: string) => `enc:${v}`,
  decrypt: (v: unknown) => (typeof v === 'string' && v.startsWith('enc:') ? v.slice(4) : v),
}));
vi.mock('./oidcDiscovery.js', () => ({
  getDiscovery: vi.fn(),
  makeInsecureFetch: vi.fn(() => vi.fn()),
}));
vi.mock('./oidcEnvConfig.js', () => ({ getMailConfigForSlug: vi.fn() }));

const { query } = vi.mocked(await import('./db.js'));
const { getDiscovery } = vi.mocked(await import('./oidcDiscovery.js'));
const { getMailConfigForSlug } = vi.mocked(await import('./oidcEnvConfig.js'));
const { provisionOidcMailAccount, refreshOidcAccountToken } = await import('./oidcMailAuth.js');

const provider = { id: 'provider-1', name: 'Corp SSO', slug: 'corp-sso' };
const mailConfig = {
  imapHost: 'imap.example.com', imapPort: 993, imapTls: true,
  smtpHost: 'smtp.example.com', smtpPort: 587, smtpTls: 'STARTTLS',
  usernameClaim: 'email', skipTlsVerify: false, displayName: null,
};

describe('provisionOidcMailAccount', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    query.mockResolvedValue({ rows: [] });
  });

  it('does nothing when the provider has no mail config', async () => {
    getMailConfigForSlug.mockReturnValue(null);
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: { email: 'a@example.com' }, email: 'a@example.com',
      tokenData: { access_token: 'at', expires_in: 3600 },
    });
    expect(query).not.toHaveBeenCalled();
  });

  it('does nothing without an access token', async () => {
    getMailConfigForSlug.mockReturnValue(mailConfig);
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: {}, email: 'a@example.com', tokenData: {},
    });
    expect(query).not.toHaveBeenCalled();
  });

  it('skips silently when the username claim is missing from the payload', async () => {
    getMailConfigForSlug.mockReturnValue({ ...mailConfig, usernameClaim: 'preferred_username' });
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: {}, email: 'a@example.com',
      tokenData: { access_token: 'at', expires_in: 3600 },
    });
    expect(query).not.toHaveBeenCalled();
  });

  it('skips silently when there is no verified email', async () => {
    getMailConfigForSlug.mockReturnValue(mailConfig);
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: { email: 'a@example.com' }, email: null,
      tokenData: { access_token: 'at', expires_in: 3600 },
    });
    expect(query).not.toHaveBeenCalled();
  });

  it('upserts an email_accounts row keyed on (user, oidc_provider_id) with encrypted tokens', async () => {
    getMailConfigForSlug.mockReturnValue(mailConfig);
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: { email: 'a@example.com' }, email: 'a@example.com',
      tokenData: { access_token: 'access-tok', refresh_token: 'refresh-tok', expires_in: 3600 },
    });

    expect(query).toHaveBeenCalledTimes(1);
    const [sql, params] = query.mock.calls[0];
    expect(sql).toMatch(/ON CONFLICT \(user_id, oidc_provider_id\)/);
    expect(params).toEqual([
      'u1', 'Corp SSO', 'a@example.com', '#0ea5e9',
      'imap.example.com', 993, true, false,
      'smtp.example.com', 587, 'STARTTLS',
      'a@example.com',
      'enc:access-tok',
      'enc:refresh-tok',
      expect.any(Date),
      'provider-1',
    ]);
  });

  it('uses mail.displayName for the mailbox name when set, instead of the provider name', async () => {
    getMailConfigForSlug.mockReturnValue({ ...mailConfig, displayName: 'Company Mail' });
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: { email: 'a@example.com' }, email: 'a@example.com',
      tokenData: { access_token: 'access-tok', expires_in: 3600 },
    });

    const [, params] = query.mock.calls[0];
    expect(params![1]).toBe('Company Mail');
  });

  it('resolves a non-default username claim for auth_user while email_address stays the verified email', async () => {
    getMailConfigForSlug.mockReturnValue({ ...mailConfig, usernameClaim: 'preferred_username' });
    await provisionOidcMailAccount({
      userId: 'u1', provider,
      payload: { email: 'a@example.com', preferred_username: 'ALICE' },
      email: 'a@example.com',
      tokenData: { access_token: 'at', expires_in: 3600 },
    });
    const [, params] = query.mock.calls[0];
    expect(params![2]).toBe('a@example.com'); // email_address
    expect(params![11]).toBe('alice'); // auth_user (lowercased claim value)
  });

  it('stores a null refresh token when the provider does not return one', async () => {
    getMailConfigForSlug.mockReturnValue(mailConfig);
    await provisionOidcMailAccount({
      userId: 'u1', provider, payload: { email: 'a@example.com' }, email: 'a@example.com',
      tokenData: { access_token: 'at', expires_in: 3600 },
    });
    const [, params] = query.mock.calls[0];
    expect(params![13]).toBeNull();
  });
});

describe('refreshOidcAccountToken', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const account = {
    id: 'acct-1',
    oidc_provider_id: 'provider-1',
    oauth_refresh_token: 'enc:old-refresh',
  };

  it('throws when the account has no oidc_provider_id', async () => {
    await expect(refreshOidcAccountToken({ id: 'x', oauth_refresh_token: 'enc:r' }))
      .rejects.toThrow(/not linked to an OIDC provider/);
  });

  it('throws when the provider row no longer exists', async () => {
    query.mockResolvedValueOnce({ rows: [] });
    await expect(refreshOidcAccountToken(account)).rejects.toThrow(/no longer exists/);
  });

  it('throws when the stored refresh token cannot be decrypted', async () => {
    query.mockResolvedValueOnce({
      rows: [{ id: 'provider-1', issuer_url: 'https://idp.example.com', client_id: 'cid', client_secret: 'enc:secret', allow_insecure: false }],
    });
    await expect(refreshOidcAccountToken({ ...account, oauth_refresh_token: null }))
      .rejects.toThrow(/refresh token is missing/);
  });

  it('posts a refresh_token grant and persists the new tokens', async () => {
    query.mockResolvedValueOnce({
      rows: [{ id: 'provider-1', issuer_url: 'https://idp.example.com', client_id: 'cid', client_secret: 'enc:secret', allow_insecure: false }],
    });
    getDiscovery.mockResolvedValue({ doc: { token_endpoint: 'https://idp.example.com/token' }, jwks: undefined as never });
    const fetchMock = vi.fn(async (_url: string, _opts: { body: string }) => ({
      ok: true,
      json: async () => ({ access_token: 'new-access', refresh_token: 'new-refresh', expires_in: 3600 }),
    }));
    vi.stubGlobal('fetch', fetchMock);
    query.mockResolvedValueOnce({ rows: [] }); // the UPDATE

    const result = await refreshOidcAccountToken(account);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, opts] = fetchMock.mock.calls[0];
    expect(url).toBe('https://idp.example.com/token');
    const body = new URLSearchParams(opts.body);
    expect(body.get('grant_type')).toBe('refresh_token');
    expect(body.get('refresh_token')).toBe('old-refresh');
    expect(body.get('client_id')).toBe('cid');
    expect(body.get('client_secret')).toBe('secret');

    const [updateSql, updateParams] = query.mock.calls[1];
    expect(updateSql).toMatch(/UPDATE email_accounts SET/);
    expect(updateParams).toEqual(['enc:new-access', 'enc:new-refresh', expect.any(Date), 'acct-1']);

    expect(result.oauth_access_token).toBe('new-access');
  });

  it('throws with the provider error message when the refresh request fails', async () => {
    query.mockResolvedValueOnce({
      rows: [{ id: 'provider-1', issuer_url: 'https://idp.example.com', client_id: 'cid', client_secret: 'enc:secret', allow_insecure: false }],
    });
    getDiscovery.mockResolvedValue({ doc: { token_endpoint: 'https://idp.example.com/token' }, jwks: undefined as never });
    vi.stubGlobal('fetch', vi.fn(async () => ({
      ok: false,
      json: async () => ({ error: 'invalid_grant', error_description: 'Refresh token expired' }),
    })));

    await expect(refreshOidcAccountToken(account)).rejects.toThrow('Refresh token expired');
  });
});
