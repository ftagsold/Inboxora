import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../services/db.js', () => ({ query: vi.fn(), pool: { connect: vi.fn() } }));
vi.mock('../index.js', () => ({ imapManager: {} }));
vi.mock('../services/encryption.js', () => ({ decrypt: (value: unknown) => value, encrypt: (value: unknown) => value }));
vi.mock('../services/pushNotifications.js', () => ({ pushConfigured: false }));
vi.mock('../services/hostValidation.js', () => ({ validateHost: vi.fn(), resolveForConnection: vi.fn() }));
vi.mock('../services/connectionPolicy.js', () => ({ getConnectionPolicy: vi.fn() }));
vi.mock('../services/authLimiter.js', () => ({ authLimiterConfig: { maxRequests: 1000, windowMs: 900000 } }));
vi.mock('../services/authEvents.js', () => ({ logAuthEvent: vi.fn() }));
vi.mock('../services/mailer.js', () => ({ sendSystemEmail: vi.fn() }));
vi.mock('./oidc.js', () => ({ buildEndSessionUrl: vi.fn() }));
vi.mock('../services/categorizer.js', () => ({ invalidateGlobalCategorizationCache: vi.fn() }));
vi.mock('../services/redis.js', () => ({ redisClient: {} }));
vi.mock('../services/rateLimiter.js', () => ({ consume: vi.fn(async () => ({ limited: false })), reset: vi.fn() }));

import express from 'express';
import session from 'express-session';
import { query as __mock_query, pool } from '../services/db.js';
import authRouter from './auth.js';
import { passwordLoginDisabledByEnv } from '../services/passwordLogin.js';
import { listeningPort } from '../test/net.js';

const query = vi.mocked(__mock_query);

async function call(method: string, path: string, body?: unknown) {
  const app = express();
  app.use(express.json());
  app.use(session({ secret: 'test-session-secret', resave: false, saveUninitialized: false }));
  app.use('/api/auth', authRouter);
  const server = app.listen(0);
  try {
    const response = await fetch(`http://127.0.0.1:${listeningPort(server)}/api/auth${path}`, {
      method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
}

beforeEach(() => {
  query.mockReset().mockResolvedValue({ rows: [] }); // admin setting: password login enabled
  vi.mocked(pool.connect).mockReset();
});
afterEach(() => { vi.unstubAllEnvs(); });

describe('PASSWORD_LOGIN_DISABLED', () => {
  it('parses common truthy spellings only', () => {
    for (const value of ['true', 'TRUE', '1', 'yes', 'on', ' true ']) expect(passwordLoginDisabledByEnv({ PASSWORD_LOGIN_DISABLED: value })).toBe(true);
    for (const value of ['', 'false', '0', 'no', 'off', undefined]) expect(passwordLoginDisabledByEnv({ PASSWORD_LOGIN_DISABLED: value })).toBe(false);
  });

  it('rejects password login before looking up the user', async () => {
    vi.stubEnv('PASSWORD_LOGIN_DISABLED', 'true');
    const result = await call('POST', '/login', { username: 'ada', password: 'correct horse' });
    expect(result.status).toBe(403);
    expect(query.mock.calls.some(([sql]) => sql.includes('FROM users'))).toBe(false);
  });

  it('rejects registration, including the first account, without opening a transaction', async () => {
    vi.stubEnv('PASSWORD_LOGIN_DISABLED', 'true');
    const result = await call('POST', '/register', { username: 'ada', password: 'correct horse battery' });
    expect(result.status).toBe(403);
    expect(pool.connect).not.toHaveBeenCalled();
  });

  it('rejects password reset requests', async () => {
    vi.stubEnv('PASSWORD_LOGIN_DISABLED', 'true');
    const result = await call('POST', '/forgot-password', { email: 'ada@example.test' });
    expect(result.status).toBe(403);
  });

  it('tells the login page that password sign-in and registration are off', async () => {
    vi.stubEnv('PASSWORD_LOGIN_DISABLED', 'true');
    query.mockResolvedValue({ rows: [{ key: 'registration_open', value: 'true' }, { key: 'internal_auth_disabled', value: 'false' }] });
    const result = await call('GET', '/registration-status');
    expect(result.body).toEqual({ open: false, internalAuthDisabled: true });
  });

  it('leaves password login to the admin setting when unset', async () => {
    query.mockResolvedValue({ rows: [{ key: 'registration_open', value: 'true' }, { key: 'internal_auth_disabled', value: 'false' }] });
    const result = await call('GET', '/registration-status');
    expect(result.body).toEqual({ open: true, internalAuthDisabled: false });
  });
});
