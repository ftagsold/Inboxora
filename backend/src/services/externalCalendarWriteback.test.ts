import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DbQueryResult, DbRow } from './db.js';

type QueryMock = (text: string, params?: unknown[]) => Promise<DbQueryResult<DbRow>>;

const { query, putCalendarEvent, deleteCalendarEvent, getConnectionPolicy } = vi.hoisted(() => ({
  query: vi.fn<QueryMock>(),
  putCalendarEvent: vi.fn(),
  deleteCalendarEvent: vi.fn(),
  getConnectionPolicy: vi.fn(async () => ({ allowPrivateHosts: false })),
}));
vi.mock('./db.js', () => ({ query }));
vi.mock('./connectionPolicy.js', () => ({ getConnectionPolicy }));
vi.mock('./caldavClient.js', () => ({ putCalendarEvent, deleteCalendarEvent }));
vi.mock('./encryption.js', () => ({
  encrypt: (value: string) => `enc:v1:${value}`,
  decrypt: (value: string) => (typeof value === 'string' && value.startsWith('enc:v1:') ? value.slice('enc:v1:'.length) : value || null),
}));

import { pushEventToRemote } from './externalCalendarWriteback.js';

const writableCalendar = { id: 'cal-1', source: 'caldav', read_only: false, external_url: 'source:source-1', user_id: 'user-1' };
const source = {
  id: 'source-1', kind: 'caldav', url: 'enc:v1:https://cloud.example.com/dav/calendars/personal/',
  username: 'brmiller', password: 'enc:v1:app-password', writable: true, collections: null,
};

describe('pushEventToRemote', () => {
  beforeEach(() => { query.mockReset(); putCalendarEvent.mockReset(); deleteCalendarEvent.mockReset(); });

  it('skips a local calendar', async () => {
    const result = await pushEventToRemote({ calendar: { ...writableCalendar, source: 'local' }, uid: 'e1', action: 'put', rawIcal: 'x' });
    expect(result).toEqual({ skipped: true });
    expect(query).not.toHaveBeenCalled();
  });

  it('skips a read-only caldav calendar', async () => {
    const result = await pushEventToRemote({ calendar: { ...writableCalendar, read_only: true }, uid: 'e1', action: 'put', rawIcal: 'x' });
    expect(result).toEqual({ skipped: true });
    expect(query).not.toHaveBeenCalled();
  });

  it('skips when the source is not marked writable', async () => {
    query.mockResolvedValueOnce({ rows: [{ ...source, writable: false }] });
    const result = await pushEventToRemote({ calendar: writableCalendar, uid: 'e1', action: 'put', rawIcal: 'x' });
    expect(result).toEqual({ skipped: true });
  });

  it('creates a new event with If-None-Match and stores the returned identity', async () => {
    query
      .mockResolvedValueOnce({ rows: [source] }) // source lookup
      .mockResolvedValueOnce({ rows: [] }) // no existing remote_href/etag
      .mockResolvedValueOnce({ rows: [] }); // UPDATE remote_href/remote_etag
    putCalendarEvent.mockResolvedValueOnce({ ok: true, etag: 'new-etag' });

    const result = await pushEventToRemote({ calendar: writableCalendar, uid: 'e1', action: 'put', rawIcal: 'BEGIN:VCALENDAR...' });

    expect(result).toEqual({ ok: true });
    expect(putCalendarEvent).toHaveBeenCalledWith(expect.objectContaining({
      href: 'https://cloud.example.com/dav/calendars/personal/e1.ics',
      ifMatchEtag: null,
      username: 'brmiller', password: 'app-password',
    }));
    const updateCall = query.mock.calls.at(-1);
    expect(updateCall?.[0]).toContain('UPDATE calendar_events SET remote_href');
    expect(updateCall?.[1]).toEqual(['enc:v1:https://cloud.example.com/dav/calendars/personal/e1.ics', 'new-etag', 'cal-1', 'e1', '']);
  });

  it('on a PUT conflict, applies the server version locally and reports it without throwing', async () => {
    query
      .mockResolvedValueOnce({ rows: [source] })
      .mockResolvedValueOnce({ rows: [{ remote_href: 'enc:v1:https://cloud.example.com/dav/calendars/personal/e1.ics', remote_etag: 'stale' }] })
      .mockResolvedValueOnce({ rows: [] }); // applyRemoteConflict's upsert
    putCalendarEvent.mockResolvedValueOnce({
      ok: false, conflict: true,
      remote: { ical: 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e1\r\nDTSTART:20260901T090000Z\r\nDTEND:20260901T100000Z\r\nSUMMARY:Server Version\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n', etag: 'server-etag' },
    });

    const result = await pushEventToRemote({ calendar: writableCalendar, uid: 'e1', action: 'put', rawIcal: 'BEGIN:VCALENDAR...MINE' });

    expect(result).toEqual({ ok: false, conflict: true, message: expect.stringContaining('newer version') });
    const upsertCall = query.mock.calls.at(-1);
    expect(upsertCall?.[0]).toContain('INSERT INTO calendar_events');
    expect(upsertCall?.[1]?.[5]).toBe('Server Version'); // summary from the server's version
  });

  it('a delete with no remote_href yet is a no-op success (never pushed before)', async () => {
    query
      .mockResolvedValueOnce({ rows: [source] })
      .mockResolvedValueOnce({ rows: [{ remote_href: null, remote_etag: null }] });

    const result = await pushEventToRemote({ calendar: writableCalendar, uid: 'e1', action: 'delete' });

    expect(result).toEqual({ ok: true });
    expect(deleteCalendarEvent).not.toHaveBeenCalled();
  });

  // Regression: the DELETE route removes the local calendar_events row (and the
  // remote_href/remote_etag it carried) before this runs, so a post-delete lookup
  // always finds nothing and the remote resource was silently never deleted — the
  // event kept reappearing on the next pull sync. The route now passes the identity
  // it captured beforehand instead of leaving this to look it up.
  it('deletes on the remote using a pre-fetched identity, without querying calendar_events for it', async () => {
    query.mockResolvedValueOnce({ rows: [source] }); // only the source lookup — no event-row query
    deleteCalendarEvent.mockResolvedValueOnce({ ok: true, etag: null });

    const result = await pushEventToRemote({
      calendar: writableCalendar, uid: 'e1', action: 'delete',
      remote: { href: 'https://cloud.example.com/dav/calendars/personal/e1.ics', etag: 'known-etag' },
    });

    expect(result).toEqual({ ok: true });
    expect(query).toHaveBeenCalledTimes(1);
    expect(deleteCalendarEvent).toHaveBeenCalledWith(expect.objectContaining({
      href: 'https://cloud.example.com/dav/calendars/personal/e1.ics',
      ifMatchEtag: 'known-etag',
    }));
  });

  it('a pre-fetched identity with no href is still a no-op success (never pushed before)', async () => {
    query.mockResolvedValueOnce({ rows: [source] });

    const result = await pushEventToRemote({ calendar: writableCalendar, uid: 'e1', action: 'delete', remote: { href: null, etag: null } });

    expect(result).toEqual({ ok: true });
    expect(deleteCalendarEvent).not.toHaveBeenCalled();
  });

  it('surfaces a network error without throwing', async () => {
    query
      .mockResolvedValueOnce({ rows: [source] })
      .mockResolvedValueOnce({ rows: [] });
    putCalendarEvent.mockRejectedValueOnce(new Error('Could not reach the CalDAV server'));

    const result = await pushEventToRemote({ calendar: writableCalendar, uid: 'e1', action: 'put', rawIcal: 'x' });

    expect(result).toEqual({ ok: false, conflict: false, error: 'Could not reach the CalDAV server' });
  });
});
