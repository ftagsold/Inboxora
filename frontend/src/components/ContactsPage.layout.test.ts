import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const source = await readFile(new URL('./ContactsPage.tsx', import.meta.url), 'utf8');

test('desktop Contacts uses its own independent list width and fills its detail pane', () => {
  assert.match(source, /data-testid="contacts-desktop-list"[\s\S]*?flex: ['"]0 0 var\(--contacts-list-width\)['"][\s\S]*?width: ['"]var\(--contacts-list-width\)['"]/);
  assert.match(source, /data-testid="contacts-desktop-detail"[\s\S]*?flex: 1/);
  assert.doesNotMatch(source, /maxWidth: 560/);
  assert.doesNotMatch(source, /var\(--list-width\)/);
});

test('desktop Contacts resizes its list with its own panel handle, independent of Mail', () => {
  assert.match(source, /import \{ applyContactsListWidth, beginContactsListResize, readContactsListWidth \} from '\.\.\/utils\/panelWidth\.ts'/);
  assert.match(source, /<PanelResizeHandle testId="contacts-list-resize" onMouseDown=\{handleListResizeMouseDown\} \/>/);
  assert.match(source, /beginContactsListResize\(event, \{ edge: 'right' \}\)/);
  assert.doesNotMatch(source, /\bbeginPanelResize\(/);
});

test('mobile Contacts puts creation in the shared header without reserving floating-button space', () => {
  assert.match(source, /data-testid="contacts-header-new"/);
  assert.doesNotMatch(source, /contacts-mobile-fab/);
});

test('contact detail renders imported events as contact dates without exposing raw Google CSV columns', () => {
  assert.match(source, /const contactDates = c\.contactDates\?\.length/);
  assert.match(source, /\{contactDates\.map\(\(date, i\)/);
  assert.doesNotMatch(source, /Object\.entries\(c\.googleFields \|\| \{\}\)\.map/);
});
