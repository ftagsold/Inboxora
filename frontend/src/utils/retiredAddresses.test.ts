import assert from 'node:assert/strict';
import test from 'node:test';
import { normalizeRetiredAddresses, retiredAddressOf } from './retiredAddresses.ts';

test('retired addresses are trimmed, lowercased, de-duplicated and validated', () => {
  assert.deepEqual(normalizeRetiredAddresses([' Old@GMX.de ', 'old@gmx.de', 'nope', '', 'x@y.io']), ['old@gmx.de', 'x@y.io']);
  assert.deepEqual(normalizeRetiredAddresses('old@gmx.de'), []);
});

test('a mail matches through To, Cc or the delivery headers in any stored shape', () => {
  const retired = ['old@gmx.de'];
  assert.equal(retiredAddressOf(retired, JSON.stringify([{ name: 'Me', email: 'Old@GMX.de' }])), 'old@gmx.de');
  assert.equal(retiredAddressOf(retired, [], [{ address: 'old@gmx.de' }]), 'old@gmx.de');
  assert.equal(retiredAddressOf(retired, null, null, ['new@x.io', 'old@gmx.de']), 'old@gmx.de');
  assert.equal(retiredAddressOf(retired, ['Me <old@gmx.de>']), 'old@gmx.de');
  assert.equal(retiredAddressOf(retired, [{ email: 'new@x.io' }]), null);
  assert.equal(retiredAddressOf([], [{ email: 'old@gmx.de' }]), null);
});
