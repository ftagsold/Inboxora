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
