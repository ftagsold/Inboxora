import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';

test('the search-icon rule cannot reach the clear button icon', () => {
  const css = readFileSync(new URL('./calendar.css', import.meta.url), 'utf8');
  // `.calendar-search svg:first-child` also matched the svg inside the clear button and
  // absolutely positioned it 9px past the button, outside the input.
  assert.doesNotMatch(css, /\.calendar-search svg:first-child/);
  assert.match(css, /\.calendar-search > svg \{[^}]*position: absolute/);
  assert.match(css, /\.calendar-search button \{[^}]*position: absolute; right: 6px/);
});
