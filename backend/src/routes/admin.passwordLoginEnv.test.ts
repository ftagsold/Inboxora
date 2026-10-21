import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../services/db.js', () => ({ query: vi.fn() }));
vi.mock('../index.js', () => ({ imapManager: {} }));
vi.mock('../services/encryption.js', () => ({ decrypt: (value: unknown) => value, encrypt: (value: unknown) => value }));
vi.mock('../services/hostValidation.js', () => ({ validateHost: vi.fn(), resolveForConnection: vi.fn() }));
vi.mock('../services/smtpTransport.js', () => ({ createSmtpTransport: vi.fn() }));
vi.mock('../services/connectionPolicy.js', () => ({ getConnectionPolicy: vi.fn(), invalidateConnectionPolicyCache: vi.fn() }));
vi.mock('../services/authLimiter.js', () => ({ reloadAuthSettings: vi.fn() }));
vi.mock('../services/externalCalendarSync.js', () => ({ stopCalendarSource: vi.fn() }));
vi.mock('../services/carddavCollectionSync.js', () => ({ stopCarddavSide: vi.fn() }));
vi.mock('../plugins/registry.js', () => ({ pluginRegistry: {} }));

import express from 'express';
import session from 'express-session';
import { query as __mock_query } from '../services/db.js';
import adminRouter from './admin.js';
import { listeningPort } from '../test/net.js';

const query = vi.mocked(__mock_query);

async function call(method: string, path: string, body?: unknown) {
  const app = express();
  app.use(express.json());
  app.use(session({ secret: 'test-session-secret', resave: false, saveUninitialized: true }));
  app.use((req, _res, next) => { req.session.userId = 'admin-1'; next(); });
  app.use('/api/admin', adminRouter);
  const server = app.listen(0);
  try {
    const response = await fetch(`http://127.0.0.1:${listeningPort(server)}/api/admin${path}`, {
      method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
}

beforeEach(() => {
  query.mockReset().mockImplementation(async (sql: string) => {
    if (sql.includes('SELECT is_admin')) return { rows: [{ is_admin: true }] };
    if (sql.includes('FROM system_settings')) return { rows: [{ key: 'internal_auth_disabled', value: 'false' }] };
    return { rows: [] };
  });
});
afterEach(() => { vi.unstubAllEnvs(); });

describe('admin settings under PASSWORD_LOGIN_DISABLED', () => {
  it('reports password login as disabled and locked by the environment', async () => {
    vi.stubEnv('PASSWORD_LOGIN_DISABLED', 'true');
    const result = await call('GET', '/settings');
    expect(result.body).toMatchObject({ settings: { internal_auth_disabled: 'true' }, passwordLoginLockedByEnv: true });
  });

  it('refuses to re-enable password login and stores nothing', async () => {
    vi.stubEnv('PASSWORD_LOGIN_DISABLED', 'true');
    const result = await call('PATCH', '/settings', { internal_auth_disabled: false });
    expect(result.status).toBe(409);
    expect(query.mock.calls.some(([sql]) => sql.includes('INSERT INTO system_settings'))).toBe(false);
  });

  it('reports the stored setting unlocked when the variable is unset', async () => {
    const result = await call('GET', '/settings');
    expect(result.body).toEqual({ settings: { internal_auth_disabled: 'false' }, passwordLoginLockedByEnv: false });
  });
});
