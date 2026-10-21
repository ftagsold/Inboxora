import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
registerHooks({ load(url, context, nextLoad) {
  if (url.endsWith('.json')) return { format: 'module', source: `export default ${readFileSync(new URL(url), 'utf8')}`, shortCircuit: true };
  return nextLoad(url, context);
} });

// A fresh device with no saved preference: nothing in localStorage.
Reflect.set(globalThis, 'localStorage', { getItem: () => null, setItem() {}, removeItem() {} });
const { useStore } = await import('./index.ts');

test('conversation grouping and the conversation reader are on by default', () => {
  assert.equal(useStore.getState().threadedView, true);
  assert.equal(useStore.getState().conversationReaderViewEnabled, true);
});

test('conversations show the newest message first by default', () => {
  assert.equal(useStore.getState().conversationNewestFirst, true);
});

test('a saved "off" preference for conversation grouping is still honoured', () => {
  const source = readFileSync(new URL('./index.ts', import.meta.url), 'utf8');
  assert.match(source, /threadedView: localStorage\.getItem\('mailflow_threaded_view'\) !== 'false'/);
});
