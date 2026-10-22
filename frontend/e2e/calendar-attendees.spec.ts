import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('the event dialog shows each attendee answer and suggests addresses while typing', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'event dialog contract');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (/\/attendees$/.test(new URL(route.request().url()).pathname)) {
      return route.fulfill({ json: { attendees: [
        { email: 'anna@example.test', name: null, partstat: 'ACCEPTED' },
        { email: 'max@example.test', name: null, partstat: 'DECLINED' },
        { email: 'eva@example.test', name: null, partstat: 'NEEDS-ACTION' },
      ] } });
    }
    return route.fulfill({ json: { events: [{ id: 'planning', calendar_id: 'calendar-personal', source: 'local', read_only: false, summary: 'Planungsrunde', attendees: ['anna@example.test', 'max@example.test', 'eva@example.test'], invite_account_id: 'account-gmail', starts_at: '2026-09-10T09:00:00Z', ends_at: '2026-09-10T10:00:00Z' }] } });
  });
  await page.route('**/api/search/contacts**', route => route.fulfill({ json: { contacts: [{ email: 'clara@example.test', name: 'Clara Beispiel' }] } }));
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: /Planungsrunde/ }).first().dblclick();
  const dialog = page.getByTestId('calendar-event-dialog');
  const attendees = dialog.getByTestId('calendar-attendees');
  await expect(attendees).toBeVisible();
  const statuses = attendees.getByTestId('calendar-attendee-status');
  await expect(statuses).toHaveCount(3);
  await expect(statuses.nth(0)).toHaveAttribute('data-status', 'ACCEPTED');
  await expect(statuses.nth(1)).toHaveAttribute('data-status', 'DECLINED');
  await expect(statuses.nth(2)).toHaveAttribute('data-status', 'NEEDS-ACTION');

  await attendees.locator('input').fill('cla');
  await expect(page.getByText('Clara Beispiel')).toBeVisible();
  await page.getByText('Clara Beispiel').click();
  await expect(attendees).toContainText('clara@example.test');
});

test('a reply to an invitation says who answered what', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'reader coverage');
  await fixtureApi; await setupV3(page); page.__conversationMatrix = '00';
  await page.route('**/api/mail/messages/*/body**', route => route.fulfill({ json: { html: '<p>Reply</p>', text: '', calendarInvitation: true, attachments: [] } }));
  await page.route('**/api/calendar/invitations/*', route => route.fulfill({ json: { invitation: {
    method: 'REPLY', uid: 'meeting-1', summary: 'Planungsrunde', startsAt: '2026-09-10T09:00:00Z', endsAt: '2026-09-10T10:00:00Z',
    attendees: [{ email: 'max@example.test', name: 'Max Mustermann', partstat: 'DECLINED' }], recordedInCalendar: true,
  } } }));
  await page.goto('/');
  await page.locator('[data-msgid="conversation-gmail-copy-2"]:visible').click();
  const card = page.getByTestId('calendar-invitation-card').filter({ visible: true }).first();
  await expect(card).toHaveAttribute('data-invitation-method', 'REPLY');
  await expect(card.getByTestId('calendar-invitation-reply')).toHaveText('Max Mustermann · odrzucił(a)');
  await expect(card.getByRole('status')).toHaveText('Zaktualizowano w Twoim kalendarzu');
  await expect(card.getByRole('button')).toHaveCount(0);
});

test('an event with attendees reopens with its attendee field shown, framed once like the composer', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'event dialog contract');
  await fixtureApi; await setupV3(page);
  // Attendees but no recorded sender (e.g. an older event): the section still opens.
  await page.route('**/api/calendar/events**', route => {
    if (/\/attendees$/.test(new URL(route.request().url()).pathname)) return route.fulfill({ json: { attendees: [] } });
    return route.fulfill({ json: { events: [{ id: 'planning', calendar_id: 'calendar-personal', source: 'local', read_only: false, summary: 'Planungsrunde', attendees: ['anna@example.test'], invite_account_id: null, starts_at: '2026-09-10T09:00:00Z', ends_at: '2026-09-10T10:00:00Z' }] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: /Planungsrunde/ }).first().dblclick();
  const attendees = page.getByTestId('calendar-event-dialog').getByTestId('calendar-attendees');
  await expect(attendees).toBeVisible();
  await expect(attendees).toContainText('anna@example.test');
  const input = attendees.locator('input');
  await input.focus();
  const inner = await input.evaluate(element => { const style = getComputedStyle(element); return { border: style.borderTopStyle, outline: style.outlineStyle, shadow: style.boxShadow, background: style.backgroundColor }; });
  expect(inner).toEqual({ border: 'none', outline: 'none', shadow: 'none', background: 'rgba(0, 0, 0, 0)' });
});
