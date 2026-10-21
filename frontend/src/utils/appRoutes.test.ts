import assert from 'node:assert/strict';
import test from 'node:test';
import { pathForView, viewFromPath, viewFromState } from './appRoutes.ts';

test('account folders, the unified inbox, calendar and contacts have stable paths', () => {
  assert.equal(pathForView({ kind: 'mail', accountId: 'acc-1', folder: 'INBOX' }), '/mail/acc-1/INBOX');
  assert.equal(pathForView({ kind: 'mail', accountId: null, folder: 'INBOX' }), '/mail/all/INBOX');
  assert.equal(pathForView({ kind: 'calendar' }), '/calendar');
  assert.equal(pathForView({ kind: 'contacts' }), '/contacts');
});

test('nested and non-ASCII folder paths round-trip as one segment', () => {
  for (const folder of ['INBOX/Projects/2026', 'Gesendete Objekte', 'Archiv.Rechnungen', '[Gmail]/All Mail', '100% done']) {
    const view = { kind: 'mail' as const, accountId: 'acc-1', folder };
    const path = pathForView(view);
    assert.equal(path.split('/').length, 4, path);
    assert.deepEqual(viewFromPath(path), view);
  }
});

test('paths that name no view are ignored', () => {
  for (const path of ['/', '/login', '/mail', '/calendar/x', '/mail/acc/INBOX/extra', '/mail/acc/%E0%A4%A']) {
    assert.equal(viewFromPath(path), null, path);
  }
  assert.deepEqual(viewFromPath('/mail/acc-1'), { kind: 'mail', accountId: 'acc-1', folder: 'INBOX' });
});

test('the store view prefers calendar and contacts over the mail selection', () => {
  const mail = { showCalendar: false, showContacts: false, selectedAccountId: 'acc-1', selectedFolder: 'Sent' };
  assert.deepEqual(viewFromState(mail), { kind: 'mail', accountId: 'acc-1', folder: 'Sent' });
  assert.deepEqual(viewFromState({ ...mail, showCalendar: true }), { kind: 'calendar' });
  assert.deepEqual(viewFromState({ ...mail, showContacts: true }), { kind: 'contacts' });
});
