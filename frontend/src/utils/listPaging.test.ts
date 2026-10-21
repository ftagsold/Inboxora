import assert from 'node:assert/strict';
import test from 'node:test';
import { hasMorePages } from './listPaging.ts';

test('a full page with more reported means there is more', () => {
  assert.equal(hasMorePages(50, 50, 50, 120), true);
});

test('a short page ends the list even when the folder count is larger', () => {
  assert.equal(hasMorePages(37, 50, 87, 120), false);
  assert.equal(hasMorePages(0, 50, 50, 120), false);
});

test('a full page that reaches the reported total ends the list', () => {
  assert.equal(hasMorePages(50, 50, 100, 100), false);
});

import { anchorRefreshedRows } from './listPaging.ts';

const row = (id: string, day: number) => ({ id, date: `2026-10-${String(day).padStart(2, '0')}T10:00:00Z` });

test('new mail on top keeps every previously loaded row, including the oldest', () => {
  const previous = [row('c', 20), row('b', 19), row('a', 18)];
  const fetched = [row('new', 21), row('c', 20), row('b', 19), row('a', 18), row('older', 17)];
  assert.deepEqual(anchorRefreshedRows(previous, fetched).map(item => item.id), ['new', 'c', 'b', 'a']);
});

test('deleting the new mail does not pull in an older row', () => {
  const previous = [row('new', 21), row('c', 20), row('b', 19), row('a', 18)];
  const fetched = [row('c', 20), row('b', 19), row('a', 18), row('older', 17)];
  assert.deepEqual(anchorRefreshedRows(previous, fetched).map(item => item.id), ['c', 'b', 'a']);
});

test('when the oldest row itself was deleted, its date still bounds the list', () => {
  const previous = [row('c', 20), row('b', 19), row('a', 18)];
  const fetched = [row('c', 20), row('b', 19), row('older', 17)];
  assert.deepEqual(anchorRefreshedRows(previous, fetched).map(item => item.id), ['c', 'b']);
});

test('an empty list takes the fetched page as is', () => {
  assert.deepEqual(anchorRefreshedRows([], [row('a', 18)]).map(item => item.id), ['a']);
});
