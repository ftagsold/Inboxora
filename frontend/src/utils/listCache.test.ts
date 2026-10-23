import assert from 'node:assert/strict';
import test from 'node:test';
import type { StoreMessageRow } from '../store/index.ts';
import { carryMeta, compareRows, cursorOf, hydratePendingRows, mergeChanges, patchRows } from './listCache.ts';

const row = (id: string, day: number, extra: Partial<StoreMessageRow> = {}): StoreMessageRow =>
  ({ id, account_id: 'a1', date: `2026-10-${String(day).padStart(2, '0')}T10:00:00.000Z`, subject: `s-${id}`, ...extra });

test('a change feed updates rows in place, drops deleted ones and inserts newer mail on top', () => {
  const rows = [row('c', 20), row('b', 19, { is_read: false }), row('a', 18)];
  const merged = mergeChanges(rows, [row('new', 21), row('b', 19, { is_read: true })], ['a']);
  assert.deepEqual(merged.map(r => r.id), ['new', 'c', 'b']);
  assert.equal(merged.find(r => r.id === 'b')?.is_read, true);
});

test('changed rows older than the loaded window stay out (they belong to later pages)', () => {
  const merged = mergeChanges([row('c', 20), row('b', 19)], [row('old', 10)], []);
  assert.deepEqual(merged.map(r => r.id), ['c', 'b']);
});

test('a pending changed row never blanks the loaded subject', () => {
  const merged = mergeChanges([row('b', 19)], [row('b', 19, { subject: '', meta_pending: true, is_starred: true })], []);
  assert.equal(merged[0].subject, 's-b');
  assert.equal(merged[0].is_starred, true);
  assert.notEqual(merged[0].meta_pending, true);
});

test('rows sort newest first with ties broken by id descending', () => {
  const sorted = [row('a', 19), row('c', 20), row('b', 19)].sort(compareRows);
  assert.deepEqual(sorted.map(r => r.id), ['c', 'b', 'a']);
});

test('the cursor is the last row as ISO date and id', () => {
  assert.equal(cursorOf([row('c', 20), row('b', 19)]), '2026-10-19T10:00:00.000Z|b');
  assert.equal(cursorOf([]), null);
  assert.equal(cursorOf([{ id: 'x', account_id: 'a1', date: null }]), null);
});

test('pending rows take the display fields already on screen', () => {
  const fresh = [row('b', 19, { subject: '', meta_pending: true }), row('n', 21)];
  const out = carryMeta(fresh, [row('b', 19)]);
  assert.equal(out[0].subject, 's-b');
  assert.equal(out[0].meta_pending, false);
  assert.equal(out[1].subject, 's-n');
});

test('thread rows never take another row’s fields', () => {
  const fresh = [row('t', 19, { subject: '', meta_pending: true, message_count: 3 })];
  assert.equal(carryMeta(fresh, [row('t', 19)])[0].meta_pending, true);
});

test('patches fill display fields but leave flags alone', () => {
  const out = patchRows([row('b', 19, { subject: '', meta_pending: true, is_read: true })],
    new Map([['b', { subject: 'Hello', is_read: false }]]));
  assert.equal(out[0].subject, 'Hello');
  assert.equal(out[0].is_read, true);
  assert.equal(out[0].meta_pending, false);
});

test('pending rows are fetched in batches and applied', async () => {
  const pending = Array.from({ length: 150 }, (_, i) => row(`p${i}`, 19, { subject: '', meta_pending: true }));
  const calls: number[] = [];
  const applied = new Map<string, Partial<StoreMessageRow>>();
  await hydratePendingRows(pending, async ids => {
    calls.push(ids.length);
    return { messages: ids.map(id => ({ id, account_id: 'a1', subject: `real-${id}` })) };
  }, patches => { for (const [k, v] of patches) applied.set(k, v); });
  assert.deepEqual(calls, [100, 50]);
  assert.equal(applied.size, 150);
  assert.equal(applied.get('p7')?.subject, 'real-p7');
});
