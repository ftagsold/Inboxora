import { EventEmitter } from 'node:events';
import { expect, it, vi } from 'vitest';
vi.mock('pg', () => ({ default: { Pool: class extends EventEmitter {} } }));
vi.mock('./encryption.js', () => ({ encrypt: vi.fn(), isEncrypted: vi.fn() }));
vi.mock('./performanceMetrics.js', () => ({ recordDb: vi.fn() }));
import { pool } from './db.js';
it('handles an idle connection failure instead of crashing the process', () => {
  const log = vi.spyOn(console, 'error').mockImplementation(() => {});
  try {
    expect(() => pool.emit('error', new Error('connection terminated'))).not.toThrow();
    expect(log).toHaveBeenCalledWith('Idle PostgreSQL connection error:', 'connection terminated');
  } finally { log.mockRestore(); }
});

it('connects over TCP with a password by default', async () => {
  const { databaseConnectionConfig } = await import('./db.js');
  expect(databaseConnectionConfig({ DB_HOST: 'postgres', DB_PORT: '5433', DB_NAME: 'inboxora', DB_USER: 'inboxora', DB_PASSWORD: 'pw' }))
    .toEqual({ host: 'postgres', port: 5433, database: 'inboxora', user: 'inboxora', password: 'pw' });
});

it('passes a Unix socket directory through and needs no password for peer/ident auth', async () => {
  const { databaseConnectionConfig, isDatabaseSocket } = await import('./db.js');
  expect(isDatabaseSocket('/run/postgresql')).toBe(true);
  expect(isDatabaseSocket('postgres')).toBe(false);
  expect(isDatabaseSocket(undefined)).toBe(false);
  expect(databaseConnectionConfig({ DB_HOST: '/run/postgresql', DB_USER: 'inboxora' }))
    .toMatchObject({ host: '/run/postgresql', port: 5432, user: 'inboxora', password: undefined });
});

it('splits a socket file path into the directory and port pg expects', async () => {
  const { databaseConnectionConfig } = await import('./db.js');
  expect(databaseConnectionConfig({ DB_HOST: '/run/postgresql/.s.PGSQL.5433', DB_PORT: '5432' }))
    .toMatchObject({ host: '/run/postgresql', port: 5433 });
});
