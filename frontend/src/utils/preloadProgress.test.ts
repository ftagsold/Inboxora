import assert from 'node:assert/strict';
import test from 'node:test';
import type { PreloadStatus } from './api.ts';
import { applyPreloadEvent, overallProgress } from './preloadProgress.ts';

const status = (): PreloadStatus => ({
  ready: false,
  accounts: [
    { id: 'a', email: 'a@x', state: 'running', done: 10, total: 100 },
    { id: 'b', email: 'b@x', state: 'pending', done: 0, total: 0 },
  ],
});

test('progress events update their account and readiness', () => {
  let s = applyPreloadEvent(status(), { accountId: 'a', state: 'done', done: 100, total: 100 });
  assert.equal(s.ready, false);
  s = applyPreloadEvent(s, { accountId: 'b', state: 'failed', error: 'auth' });
  assert.equal(s.ready, true);
  assert.equal(s.accounts[1].error, 'auth');
});

test('events for unknown accounts are ignored', () => {
  const s = status();
  assert.equal(applyPreloadEvent(s, { accountId: 'zzz', state: 'done' }), s);
});

test('overall progress sums accounts and ignores failed ones', () => {
  assert.equal(overallProgress(status().accounts), 0.1);
  assert.equal(overallProgress([{ id: 'a', state: 'pending', done: 0, total: 0 }]), null);
  assert.equal(overallProgress([{ id: 'a', state: 'failed', done: 0, total: 50 }]), 1);
  assert.equal(overallProgress([
    { id: 'a', state: 'done', done: 40, total: 50 },
    { id: 'b', state: 'running', done: 25, total: 50 },
  ]), 0.75);
});
