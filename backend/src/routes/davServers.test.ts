import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { listeningPort } from '../test/net.js';
import type { Server } from 'node:http';
import type { NextFunction, Request, Response } from 'express';
import 'express-async-errors';

interface FakeQueryResult { rows: Array<Record<string, unknown>> }
type FakeQuery = (sql: string, parameters?: unknown[]) => Promise<FakeQueryResult>;
/** The sync outcomes these tests stub: success (optionally with a count) or a not-found/transport error. */
type FakeSync = (userId: string, sourceId: string) => Promise<{ ok: boolean; error?: string; eventCount?: number }>;

const {
  query, discoverCalendars, discoverAddressBooks,
  releaseCalendarSource, scheduleCalendarSource, stopCalendarSource, syncCalendarSource,
  releaseCarddavSide, scheduleCarddavSide, stopCarddavSide, syncCarddavSide,
} = vi.hoisted(() => ({
  query: vi.fn<FakeQuery>(),
  discoverCalendars: vi.fn(),
  discoverAddressBooks: vi.fn(),
  releaseCalendarSource: vi.fn(),
  scheduleCalendarSource: vi.fn(),
  stopCalendarSource: vi.fn(),
  syncCalendarSource: vi.fn<FakeSync>(async () => ({ ok: true })),
  releaseCarddavSide: vi.fn(),
  scheduleCarddavSide: vi.fn(),
  stopCarddavSide: vi.fn(),
  syncCarddavSide: vi.fn<FakeSync>(async () => ({ ok: true })),
}));
vi.mock('../services/db.js', () => ({ query }));
vi.mock('../services/encryption.js', () => ({
  encrypt: (value: string) => `enc:v1:${value}`,
  decrypt: (value: string) => typeof value === 'string' && value.startsWith('enc:v1:') ? value.slice('enc:v1:'.length) : value,
}));
vi.mock('../services/caldavClient.js', () => ({ discoverCalendars }));
vi.mock('../services/carddavClient.js', () => ({ discoverAddressBooks }));
vi.mock('../services/externalCalendarSync.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../services/externalCalendarSync.js')>();
  return { ...actual, releaseCalendarSource, scheduleCalendarSource, stopCalendarSource, syncCalendarSource };
});
vi.mock('../services/carddavCollectionSync.js', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../services/carddavCollectionSync.js')>();
  return { ...actual, releaseCarddavSide, scheduleCarddavSide, stopCarddavSide, syncCarddavSide };
});
vi.mock('../middleware/auth.js', () => ({
  requireAuth: (req: { session?: { userId?: string } }, _res: unknown, next: () => void) => { req.session = { userId: 'user-1' }; next(); },
}));

import express from 'express';
import davServersRouter from './davServers.js';

function responseRecord(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null) throw new Error('response is not an object');
  return value as Record<string, unknown>;
}

let server: Server;
let base = '';

beforeAll(async () => {
  const app = express();
  app.use(express.json());
  app.use('/api/dav/servers', davServersRouter);
  app.use((error: Error, _req: Request, res: Response, next: NextFunction) => { void next; return res.status(500).json({ error: error.message }); });
  await new Promise(resolve => { server = app.listen(0, resolve); });
  base = `http://127.0.0.1:${listeningPort(server)}`;
});
afterAll(async () => { await new Promise(resolve => server.close(resolve)); });

beforeEach(() => {
  query.mockReset().mockResolvedValue({ rows: [] });
  discoverCalendars.mockReset();
  discoverAddressBooks.mockReset();
  releaseCalendarSource.mockReset(); scheduleCalendarSource.mockReset(); stopCalendarSource.mockReset();
  syncCalendarSource.mockReset().mockResolvedValue({ ok: true });
  releaseCarddavSide.mockReset(); scheduleCarddavSide.mockReset(); stopCarddavSide.mockReset();
  syncCarddavSide.mockReset().mockResolvedValue({ ok: true });
});

describe('POST /discover', () => {
  it('returns both calendars and address books when both succeed', async () => {
    discoverCalendars.mockResolvedValueOnce([{ href: 'https://dav.example/cal/', displayName: 'Work', color: null }]);
    discoverAddressBooks.mockResolvedValueOnce([{ url: 'https://dav.example/book/', displayName: 'Contacts' }]);

    const response = await fetch(`${base}/api/dav/servers/discover`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ url: 'https://dav.example/', username: 'person', password: 'app-password' }),
    });

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      calendars: [{ href: 'https://dav.example/cal/', displayName: 'Work', color: null }],
      addressBooks: [{ href: 'https://dav.example/book/', displayName: 'Contacts' }],
    });
  });

  it('returns the calendars found alongside the address-book error when the server has no address books', async () => {
    discoverCalendars.mockResolvedValueOnce([{ href: 'https://dav.example/cal/', displayName: 'Work', color: null }]);
    discoverAddressBooks.mockRejectedValueOnce(new Error('No address books found for this account'));

    const response = await fetch(`${base}/api/dav/servers/discover`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ url: 'https://dav.example/', username: 'person', password: 'app-password' }),
    });

    expect(response.status).toBe(200);
    const payload = responseRecord(await response.json());
    expect(payload.calendars).toEqual([{ href: 'https://dav.example/cal/', displayName: 'Work', color: null }]);
    expect(payload.addressBooksError).toBe('No address books found for this account');
  });

  it('502s only when both protocols fail', async () => {
    discoverCalendars.mockRejectedValueOnce(new Error('Authentication failed'));
    discoverAddressBooks.mockRejectedValueOnce(new Error('Authentication failed'));

    const response = await fetch(`${base}/api/dav/servers/discover`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ url: 'https://dav.example/', username: 'person', password: 'app-password' }),
    });

    expect(response.status).toBe(502);
  });
});

describe('POST /', () => {
  it('rejects a create with nothing selected in either list', async () => {
    const response = await fetch(`${base}/api/dav/servers`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ displayName: 'Work', username: 'person', password: 'app-password', url: 'https://dav.example/' }),
    });

    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: 'Select at least one calendar or address book to import' });
    expect(query).not.toHaveBeenCalled();
  });

  it('creates a server with only address books selected — no calendar sync is attempted', async () => {
    query.mockImplementation(async (sql: string) => sql.includes('INSERT INTO calendar_import_sources')
      ? { rows: [{ id: 'source-1', kind: 'caldav', display_name: 'Contacts only', address_book_collections: 'enc:v1:[]' }] }
      : { rows: [] });

    const response = await fetch(`${base}/api/dav/servers`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        displayName: 'Contacts only', username: 'person', password: 'app-password', url: 'https://dav.example/',
        addressBooks: [{ href: 'https://dav.example/book/', displayName: 'Contacts' }],
      }),
    });

    expect(response.status).toBe(201);
    expect(syncCalendarSource).not.toHaveBeenCalled();
    expect(syncCarddavSide).toHaveBeenCalledWith('user-1', 'source-1');
    expect(scheduleCalendarSource).toHaveBeenCalled();
    expect(scheduleCarddavSide).toHaveBeenCalled();

    const [, insertParameters] = query.mock.calls.find(([sql]) => sql.includes('INSERT INTO calendar_import_sources'))!;
    // collections (calendars) is always stored, even empty; address_book_collections holds the selection.
    expect(insertParameters![8]).toBe('enc:v1:[]');
    expect(insertParameters![9]).toContain('Contacts');
  });

  it('creates a server with both calendars and address books, syncing both', async () => {
    query.mockImplementation(async (sql: string) => sql.includes('INSERT INTO calendar_import_sources')
      ? { rows: [{ id: 'source-1', kind: 'caldav', display_name: 'Both' }] }
      : { rows: [] });

    const response = await fetch(`${base}/api/dav/servers`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        displayName: 'Both', username: 'person', password: 'app-password', url: 'https://dav.example/',
        calendars: [{ href: 'https://dav.example/cal/', displayName: 'Work' }],
        addressBooks: [{ href: 'https://dav.example/book/', displayName: 'Contacts' }],
      }),
    });

    expect(response.status).toBe(201);
    expect(syncCalendarSource).toHaveBeenCalledWith('user-1', 'source-1');
    expect(syncCarddavSide).toHaveBeenCalledWith('user-1', 'source-1');
  });

  it('returns the persisted server with a differentiated error when only the address-book sync fails', async () => {
    syncCarddavSide.mockResolvedValueOnce({ ok: false, error: 'network unavailable' });
    query.mockImplementation(async (sql: string) => sql.includes('INSERT INTO calendar_import_sources')
      ? { rows: [{ id: 'source-1', kind: 'caldav', display_name: 'Both' }] }
      : { rows: [] });

    const response = await fetch(`${base}/api/dav/servers`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        displayName: 'Both', username: 'person', password: 'app-password', url: 'https://dav.example/',
        calendars: [{ href: 'https://dav.example/cal/', displayName: 'Work' }],
        addressBooks: [{ href: 'https://dav.example/book/', displayName: 'Contacts' }],
      }),
    });

    expect(response.status).toBe(502);
    const payload = responseRecord(await response.json());
    expect(payload.error).toBe('network unavailable');
    expect(responseRecord(payload.server).id).toBe('source-1');
  });
});

describe('DELETE /:id', () => {
  it('stops and releases both the calendar and carddav sides, and cascades both derived tables', async () => {
    query.mockResolvedValueOnce({ rows: [{ id: 'source-1', kind: 'caldav' }] }); // existing lookup
    query.mockResolvedValueOnce({ rows: [{ id: 'source-1' }] }); // delete

    const response = await fetch(`${base}/api/dav/servers/source-1`, { method: 'DELETE' });

    expect(response.status).toBe(204);
    expect(stopCalendarSource).toHaveBeenCalledWith('source-1');
    expect(stopCarddavSide).toHaveBeenCalledWith('source-1');
    expect(releaseCalendarSource).toHaveBeenCalledWith('source-1');
    expect(releaseCarddavSide).toHaveBeenCalledWith('source-1');
    expect(query.mock.calls.some(([sql]) => typeof sql === 'string' && sql.includes('DELETE FROM address_books'))).toBe(true);
    expect(query.mock.calls.some(([sql]) => typeof sql === 'string' && sql.includes('DELETE FROM calendars'))).toBe(true);
  });

  it('404s for an unknown server', async () => {
    query.mockResolvedValueOnce({ rows: [] });
    const response = await fetch(`${base}/api/dav/servers/missing`, { method: 'DELETE' });
    expect(response.status).toBe(404);
    expect(stopCalendarSource).not.toHaveBeenCalled();
  });
});

describe('POST /:id/sync', () => {
  it('404s only when both concerns report not found', async () => {
    syncCalendarSource.mockResolvedValueOnce({ ok: false, error: 'Calendar source not found' });
    syncCarddavSide.mockResolvedValueOnce({ ok: false, error: 'DAV server not found' });

    const response = await fetch(`${base}/api/dav/servers/missing/sync`, { method: 'POST' });

    expect(response.status).toBe(404);
  });

  it('returns a combined result when at least one concern is found', async () => {
    syncCalendarSource.mockResolvedValueOnce({ ok: true, eventCount: 3 });
    syncCarddavSide.mockResolvedValueOnce({ ok: false, error: 'DAV server not found' });

    const response = await fetch(`${base}/api/dav/servers/source-1/sync`, { method: 'POST' });

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      calendarSync: { ok: true, eventCount: 3 },
      addressBookSync: { ok: false, error: 'DAV server not found' },
    });
  });
});
