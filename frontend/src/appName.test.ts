import assert from 'node:assert/strict';
import test from 'node:test';
import { APP_NAME, APP_NAME_PLACEHOLDER, runtimeAppName } from './appName.ts';

const page = (content: string | null) => ({
  querySelector: (selector: string) => (selector === 'meta[name="application-name"]' && content !== null
    ? { getAttribute: (name: string) => (name === 'content' ? content : null) } as unknown as Element
    : null),
});

test('reads the name the container filled into index.html', () => {
  assert.equal(runtimeAppName(page('KnödelMail')), 'KnödelMail');
  assert.equal(runtimeAppName(page('  Spaced  ')), 'Spaced');
});

test('ignores an unprocessed placeholder, a missing tag and a missing document', () => {
  assert.equal(runtimeAppName(page(APP_NAME_PLACEHOLDER)), null);
  assert.equal(runtimeAppName(page('')), null);
  assert.equal(runtimeAppName(page(null)), null);
  assert.equal(runtimeAppName(undefined), null);
  assert.equal(APP_NAME, 'Inboxora'); // node:test has no document
});
