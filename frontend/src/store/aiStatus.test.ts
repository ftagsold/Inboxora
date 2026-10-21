import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
registerHooks({ load(url, context, nextLoad) {
  if (url.endsWith('.json')) return { format: 'module', source: `export default ${readFileSync(new URL(url), 'utf8')}`, shortCircuit: true };
  return nextLoad(url, context);
} });
Reflect.set(globalThis, 'localStorage', { getItem: () => null, setItem() {}, removeItem() {} });
const { useStore } = await import('./index.ts');
const { api } = await import('../utils/api.ts');

test('AI is treated as unavailable until the status loads, and when the request fails', async () => {
  useStore.setState({ user: { id: 'u1' }, aiStatus: null });
  assert.equal(useStore.getState().aiStatus?.enabled ?? false, false);
  const original = api.ai.status;
  api.ai.status = async () => { throw new Error('offline'); };
  try {
    await useStore.getState().refreshAiStatus();
    assert.deepEqual(useStore.getState().aiStatus, { enabled: false });
  } finally { api.ai.status = original; }
});

test('a status response that arrives after the user changed is dropped', async () => {
  useStore.setState({ user: { id: 'u1' }, aiStatus: null });
  let resolve: (value: unknown) => void = () => {};
  const original = api.ai.status;
  api.ai.status = () => new Promise(r => { resolve = r; });
  try {
    const pending = useStore.getState().refreshAiStatus();
    useStore.setState({ user: { id: 'u2' } });
    resolve({ enabled: true, features: { summarize: true } });
    await pending;
    assert.equal(useStore.getState().aiStatus, null);
  } finally { api.ai.status = original; }
});
