import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');

test('DAV servers gets its own settings tab, registered alongside DAV credentials', async () => {
  const admin = await source('./AdminPanel.tsx');
  assert.match(admin, /import DavServersSettings from '\.\/DavServersSettings\.tsx'/);
  assert.match(admin, /id: 'dav-servers', labelKey: 'admin\.tabs\.davServers'/);
  assert.match(admin, /tabIds: \[.*'dav-credentials', 'dav-servers'/);
  assert.match(admin, /adminTab === 'dav-servers' && <DavServersSettings \/>/);
  // The old single-account CardDAV card is gone, not just unmounted.
  assert.doesNotMatch(admin, /function CardDavCard/);
  assert.doesNotMatch(admin, /<CardDavCard \/>/);
});

test('adding a server discovers both protocols from one form and creates independently per protocol', async () => {
  const component = await source('./DavServersSettings.tsx');
  assert.match(component, /data-testid="dav-servers-settings"/);
  assert.match(component, /api\.davServers\.discover\(/);
  assert.match(component, /discovered\.calendars/);
  assert.match(component, /discovered\.addressBooks/);
  // Creation only includes a protocol's payload when something was actually selected —
  // a contacts-only or calendars-only server is a valid, expected outcome.
  assert.match(component, /api\.davServers\.create\(/);
  assert.match(component, /filter\(entry => selectedCalendars\.has\(entry\.href\)\)/);
  assert.match(component, /filter\(entry => selectedAddressBooks\.has\(entry\.href\)\)/);
});

test('existing servers list, edit, sync and delete through the unified API, not the old single-account carddav API', async () => {
  const component = await source('./DavServersSettings.tsx');
  assert.match(component, /data-testid="dav-server-row"/);
  assert.match(component, /api\.davServers\.list\(\)/);
  assert.match(component, /api\.davServers\.update\(/);
  assert.match(component, /api\.davServers\.sync\(/);
  assert.match(component, /api\.davServers\.remove\(/);
  assert.doesNotMatch(component, /api\.carddav\./);
  // The edit dialog can independently toggle/replace calendars and address books.
  assert.match(component, /toggleEditCollection\('calendars'/);
  assert.match(component, /toggleEditCollection\('addressBooks'/);
});

test('the old single-account carddav API surface is gone from api.ts', async () => {
  const api = await source('../utils/api.ts');
  assert.match(api, /davServers: \{/);
  assert.doesNotMatch(api, /carddav: \{/);
  assert.doesNotMatch(api, /'\/carddav\/connect'/);
});
