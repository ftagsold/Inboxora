import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { safeFetch, validateHost } = vi.hoisted(() => ({
  safeFetch: vi.fn<(url: string, options: RequestInit, policy: { allowPrivate?: boolean }) => Promise<Response>>(),
  validateHost: vi.fn<() => Promise<string | null>>().mockResolvedValue(null),
}));
vi.mock('./safeFetch.js', () => ({ safeFetch }));
vi.mock('./hostValidation.js', () => ({ validateHost }));

import { extractHref, parseCalendars, putCalendarEvent, deleteCalendarEvent, discoverCalendars } from './caldavClient.js';

const BASE = 'https://cloud.example.com/remote.php/dav/calendars/brmiller/';

describe('extractHref (discovery)', () => {
  it('pulls current-user-principal href, resolving relative to origin', () => {
    const xml = `<d:multistatus xmlns:d="DAV:">
      <d:response><d:href>/remote.php/dav/</d:href>
        <d:propstat><d:prop><d:current-user-principal><d:href>/remote.php/dav/principals/users/brmiller/</d:href></d:current-user-principal></d:prop>
        <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
      </d:response></d:multistatus>`;
    expect(extractHref(xml, 'current-user-principal', 'https://cloud.example.com/remote.php/dav/'))
      .toBe('https://cloud.example.com/remote.php/dav/principals/users/brmiller/');
  });

  it('pulls calendar-home-set href across a caldav namespace prefix', () => {
    const xml = `<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
      <d:response><d:href>/remote.php/dav/principals/users/brmiller/</d:href>
        <d:propstat><d:prop><cal:calendar-home-set><d:href>/remote.php/dav/calendars/brmiller/</d:href></cal:calendar-home-set></d:prop>
        <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
      </d:response></d:multistatus>`;
    expect(extractHref(xml, 'calendar-home-set', 'https://cloud.example.com/remote.php/dav/principals/users/brmiller/'))
      .toBe('https://cloud.example.com/remote.php/dav/calendars/brmiller/');
  });

  it('returns null when the property is absent', () => {
    const xml = `<d:multistatus xmlns:d="DAV:"><d:response><d:href>/x/</d:href>
      <d:propstat><d:prop/><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response></d:multistatus>`;
    expect(extractHref(xml, 'current-user-principal', BASE)).toBeNull();
  });
});

describe('parseCalendars', () => {
  it('returns only calendar collections, resolving relative hrefs and reading the Apple calendar-color', () => {
    const xml = `<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
  <d:response>
    <d:href>/remote.php/dav/calendars/brmiller/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/brmiller/personal/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
      <d:displayname>Personal</d:displayname>
      <ic:calendar-color>#7C6AF7FF</ic:calendar-color>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>`;
    const calendars = parseCalendars(xml, BASE);
    expect(calendars).toHaveLength(1); // the plain (non-calendar) collection is excluded
    expect(calendars[0].displayName).toBe('Personal');
    expect(calendars[0].href).toBe('https://cloud.example.com/remote.php/dav/calendars/brmiller/personal/');
    expect(calendars[0].color).toBe('#7C6AF7');
  });

  it('excludes scheduling inbox/outbox collections alongside real calendars', () => {
    const xml = `<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response><d:href>/dav/calendars/brmiller/inbox/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/><cal:schedule-inbox/></d:resourcetype></d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
  <d:response><d:href>/dav/calendars/brmiller/work/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>Work</d:displayname></d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>`;
    const calendars = parseCalendars(xml, BASE);
    expect(calendars).toHaveLength(1);
    expect(calendars[0].displayName).toBe('Work');
  });

  it('defaults to a null color and "Calendar" name when absent', () => {
    const xml = `<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response><d:href>/dav/calendars/brmiller/x/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype></d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>`;
    const calendars = parseCalendars(xml, BASE);
    expect(calendars[0].displayName).toBe('Calendar');
    expect(calendars[0].color).toBeNull();
  });
});

describe('CalDAV write-back', () => {
  beforeEach(() => { safeFetch.mockReset(); validateHost.mockResolvedValue(null); });
  afterEach(() => { vi.clearAllMocks(); });
  const creds = { username: 'brmiller', password: 'app-password', allowPrivate: false };

  it('PUTs a new event with If-None-Match: * and returns the server ETag', async () => {
    safeFetch.mockResolvedValue(new Response(null, { status: 201, headers: { ETag: '"abc123"' } }));
    const result = await putCalendarEvent({ href: `${BASE}event-1.ics`, ical: 'BEGIN:VCALENDAR...', ifMatchEtag: null, ...creds });
    expect(result).toEqual({ ok: true, etag: 'abc123' });
    const [, options] = safeFetch.mock.calls[0];
    expect((options.headers as Record<string, string>)['If-None-Match']).toBe('*');
  });

  it('falls back to a GET when the PUT response carries no ETag', async () => {
    safeFetch
      .mockResolvedValueOnce(new Response(null, { status: 204 })) // PUT, no ETag header
      .mockResolvedValueOnce(new Response('ICAL', { status: 200, headers: { ETag: '"fetched-etag"' } })); // fallback GET
    const result = await putCalendarEvent({ href: `${BASE}event-1.ics`, ical: 'BEGIN:VCALENDAR...', ifMatchEtag: 'old-etag', ...creds });
    expect(result).toEqual({ ok: true, etag: 'fetched-etag' });
    expect(safeFetch).toHaveBeenCalledTimes(2);
  });

  it('reports a conflict with the server’s current version on a 412', async () => {
    safeFetch
      .mockResolvedValueOnce(new Response(null, { status: 412 })) // PUT rejected
      .mockResolvedValueOnce(new Response('BEGIN:VCALENDAR...SERVER VERSION', { status: 200, headers: { ETag: '"server-etag"' } }));
    const result = await putCalendarEvent({ href: `${BASE}event-1.ics`, ical: 'BEGIN:VCALENDAR...MINE', ifMatchEtag: 'stale-etag', ...creds });
    expect(result).toEqual({ ok: false, conflict: true, remote: { ical: 'BEGIN:VCALENDAR...SERVER VERSION', etag: 'server-etag' } });
  });

  it('treats a conflicting delete whose resource is now gone as "already removed"', async () => {
    safeFetch
      .mockResolvedValueOnce(new Response(null, { status: 412 })) // DELETE rejected
      .mockResolvedValueOnce(new Response(null, { status: 404 })); // refetch: gone
    const result = await deleteCalendarEvent({ href: `${BASE}event-1.ics`, ifMatchEtag: 'stale-etag', ...creds });
    expect(result).toEqual({ ok: false, conflict: true, remote: { ical: null, etag: null } });
  });

  it('deletes without a conditional header when no ETag is known yet', async () => {
    safeFetch.mockResolvedValue(new Response(null, { status: 204 }));
    const result = await deleteCalendarEvent({ href: `${BASE}event-1.ics`, ifMatchEtag: null, ...creds });
    expect(result).toEqual({ ok: true, etag: null });
    const [, options] = safeFetch.mock.calls[0];
    expect((options.headers as Record<string, string>)['If-Match']).toBeUndefined();
  });

  it('surfaces a non-conflict rejection as a plain error', async () => {
    safeFetch.mockResolvedValue(new Response(null, { status: 403 }));
    const result = await putCalendarEvent({ href: `${BASE}event-1.ics`, ical: 'x', ifMatchEtag: 'e', ...creds });
    expect(result).toEqual({ ok: false, conflict: false, error: 'Remote calendar rejected the write (403)' });
  });

  it('discoverCalendars fails clearly when authentication is rejected', async () => {
    safeFetch.mockResolvedValue(new Response(null, { status: 401 }));
    await expect(discoverCalendars({ serverUrl: 'https://cloud.example.com/dav/', ...creds }))
      .rejects.toThrow('Authentication failed — check the username and app password');
  });
});
