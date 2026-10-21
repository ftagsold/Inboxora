import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
registerHooks({ load(url, context, nextLoad) {
  if (url.endsWith('.json')) return { format: 'module', source: `export default ${readFileSync(new URL(url), 'utf8')}`, shortCircuit: true };
  return nextLoad(url, context);
} });
// A fresh device: no saved preference.
Reflect.set(globalThis, 'localStorage', { getItem: () => null, setItem() {}, removeItem() {} });
const { useStore } = await import('./index.ts');
const { DEFAULT_SOURCE_SYNC_INTERVAL } = await import('../components/CalendarSourceIntervalSelect.tsx');

test('mail and folder sync default to the smallest offered interval', () => {
  assert.equal(useStore.getState().syncInterval, 15);         // options: 15s, 30s, 60s, 2 min
  assert.equal(useStore.getState().folderSyncInterval, 900);  // options: 15 min, 30 min, 1 h, never
});

test('new calendar subscriptions and DAV servers default to the smallest offered interval', () => {
  assert.equal(DEFAULT_SOURCE_SYNC_INTERVAL, 15);
  for (const file of ['CalendarSubscriptionsSettings.tsx', 'DavServersSettings.tsx']) {
    const source = readFileSync(new URL(`../components/${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(source, /intervalSeconds: 3600|intervalSeconds \|\| 3600/, file);
  }
});
