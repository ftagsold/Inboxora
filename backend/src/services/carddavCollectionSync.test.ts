import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

interface FakeQueryResult { rows: Array<Record<string, unknown>> }
type FakeQuery = (sql: string, parameters?: unknown[]) => Promise<FakeQueryResult>;

const { query, decrypt, fetchAddressBookCards, getConnectionPolicy } = vi.hoisted(() => ({
  query: vi.fn<FakeQuery>(),
  decrypt: vi.fn((value: string) => value?.startsWith('enc:v1:') ? value.slice('enc:v1:'.length) : value),
  fetchAddressBookCards: vi.fn(),
  getConnectionPolicy: vi.fn(async () => ({ allowPrivateHosts: false })),
}));
vi.mock('./db.js', () => ({ query }));
vi.mock('./encryption.js', () => ({ decrypt }));
vi.mock('./connectionPolicy.js', () => ({ getConnectionPolicy }));
vi.mock('./carddavClient.js', () => ({ fetchAddressBookCards }));

import {
  parseAddressBookCollections, scheduleCarddavSide, stopCarddavSide, releaseCarddavSide, syncCarddavSide,
} from './carddavCollectionSync.js';

const VCARD_A = 'BEGIN:VCARD\r\nVERSION:3.0\r\nUID:a\r\nFN:Alice\r\nEMAIL:alice@example.com\r\nEND:VCARD\r\n';
const VCARD_B = 'BEGIN:VCARD\r\nVERSION:3.0\r\nUID:b\r\nFN:Bob\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n';

function sourceRow(overrides: Record<string, unknown> = {}) {
  return {
    id: 'source-1', user_id: 'user-1', username: 'person', password: 'enc:v1:app-password',
    interval_seconds: 3600, enabled: true,
    address_book_collections: 'enc:v1:' + JSON.stringify([
      { href: 'https://dav.example/books/personal/', hrefHash: 'hash-personal', displayName: 'Personal', selected: true },
      { href: 'https://dav.example/books/work/', hrefHash: 'hash-work', displayName: 'Work', selected: false },
    ]),
    ...overrides,
  };
}

beforeEach(() => {
  query.mockReset().mockResolvedValue({ rows: [] });
  fetchAddressBookCards.mockReset();
});
afterEach(() => { vi.useRealTimers(); });

describe('parseAddressBookCollections', () => {
  it('returns null when the column is empty', () => {
    expect(parseAddressBookCollections({ id: 's', user_id: 'u', address_book_collections: null })).toBeNull();
  });
  it('parses the encrypted JSON blob', () => {
    const source = sourceRow();
    expect(parseAddressBookCollections(source)).toEqual([
      { href: 'https://dav.example/books/personal/', hrefHash: 'hash-personal', displayName: 'Personal', selected: true },
      { href: 'https://dav.example/books/work/', hrefHash: 'hash-work', displayName: 'Work', selected: false },
    ]);
  });
});

describe('syncCarddavSide', () => {
  it('only fetches the selected collection, and writes address-book-specific status columns', async () => {
    const source = sourceRow();
    query.mockImplementation(async (sql: string) => {
      if (sql.includes('SELECT * FROM calendar_import_sources')) return { rows: [source] };
      return { rows: [{ id: 'book-1' }] };
    });
    fetchAddressBookCards.mockResolvedValueOnce([{ href: 'https://dav.example/books/personal/alice.vcf', etag: 'e1', vcard: VCARD_A }]);

    const result = await syncCarddavSide('user-1', 'source-1');

    expect(result).toEqual({ ok: true, bookCount: 1, contactCount: 1 });
    expect(fetchAddressBookCards).toHaveBeenCalledTimes(1);
    expect(fetchAddressBookCards).toHaveBeenCalledWith(expect.objectContaining({ url: 'https://dav.example/books/personal/' }));

    const statusCall = query.mock.calls.find(([sql]) => typeof sql === 'string' && sql.includes('address_book_last_sync_at'));
    expect(statusCall?.[0]).toBe('UPDATE calendar_import_sources SET address_book_last_sync_at = NOW(), address_book_last_error = NULL WHERE id = $1');
    // The calendar side's own status columns must never be touched by this sync.
    expect(query.mock.calls.some(([sql]) => typeof sql === 'string' && /\bSET last_sync_at\b/.test(sql))).toBe(false);
  });

  it('records a failure on the address-book status columns without throwing', async () => {
    const source = sourceRow();
    query.mockImplementation(async (sql: string) => sql.includes('SELECT * FROM calendar_import_sources') ? { rows: [source] } : { rows: [] });
    fetchAddressBookCards.mockRejectedValueOnce(new Error('CardDAV server did not respond'));

    const result = await syncCarddavSide('user-1', 'source-1');

    expect(result).toEqual({ ok: false, error: 'CardDAV server did not respond' });
    const statusCall = query.mock.calls.find(([sql]) => typeof sql === 'string' && sql.includes('address_book_last_error = $2'));
    expect(statusCall?.[1]).toEqual(['source-1', 'CardDAV server did not respond']);
  });

  it('prunes a local address book whose collection is no longer selected', async () => {
    const source = sourceRow();
    query.mockImplementation(async (sql: string) => {
      if (sql.includes('SELECT * FROM calendar_import_sources')) return { rows: [source] };
      if (sql.startsWith('SELECT id FROM address_books')) return { rows: [{ id: 'book-1' }] };
      return { rows: [] };
    });
    fetchAddressBookCards.mockResolvedValueOnce([]);

    await syncCarddavSide('user-1', 'source-1');

    const pruneCall = query.mock.calls.find(([sql]) => typeof sql === 'string' && sql.includes('DELETE FROM address_books'));
    expect(pruneCall?.[1]).toEqual(['user-1', 'source:source-1:%', ['source:source-1:hash-personal']]);
  });

  it('does nothing and reports zero when no collection is selected', async () => {
    const source = sourceRow({ address_book_collections: 'enc:v1:' + JSON.stringify([{ href: 'https://dav.example/books/work/', hrefHash: 'hash-work', displayName: 'Work', selected: false }]) });
    query.mockImplementation(async (sql: string) => sql.includes('SELECT * FROM calendar_import_sources') ? { rows: [source] } : { rows: [] });

    const result = await syncCarddavSide('user-1', 'source-1');

    expect(result).toEqual({ ok: true, bookCount: 0, contactCount: 0 });
    expect(fetchAddressBookCards).not.toHaveBeenCalled();
  });

  it('reports not found for a disabled or missing source', async () => {
    query.mockResolvedValueOnce({ rows: [] });
    const result = await syncCarddavSide('user-1', 'missing');
    expect(result).toEqual({ ok: false, error: 'DAV server not found' });
  });
});

describe('scheduleCarddavSide', () => {
  it('does not schedule a server with no address-book side', () => {
    vi.useFakeTimers();
    const setIntervalSpy = vi.spyOn(global, 'setInterval');
    scheduleCarddavSide({ id: 'source-2', user_id: 'user-1', interval_seconds: 3600, address_book_collections: null });
    expect(setIntervalSpy).not.toHaveBeenCalled();
  });

  it('schedules a server that has address books', () => {
    vi.useFakeTimers();
    const setIntervalSpy = vi.spyOn(global, 'setInterval');
    scheduleCarddavSide(sourceRow());
    expect(setIntervalSpy).toHaveBeenCalledTimes(1);
    expect(setIntervalSpy.mock.calls[0][1]).toBe(3600 * 1000);
  });
});

describe('stopCarddavSide / releaseCarddavSide', () => {
  it('is a no-op for a source with no timer and no in-flight sync', async () => {
    await expect(stopCarddavSide('never-scheduled')).resolves.toBeUndefined();
    expect(() => releaseCarddavSide('never-scheduled')).not.toThrow();
  });
});
