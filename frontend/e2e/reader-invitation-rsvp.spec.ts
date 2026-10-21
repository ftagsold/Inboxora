import { test, expect } from './fixtures.ts';

// A received invitation can be answered in the mail itself. The panel lists what it collides
// with in the user's calendar, and once the answer is sent the message is archived.
test('accept an invitation in the conversation reader: shows conflicts, replies, archives', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop conversation reader contract');
  await fixtureApi;
  page.__conversationMatrix = '01';

  await page.route('**/api/mail/messages/*/body**', route => route.fulfill({ json: {
    attachments: [{ part: '2', filename: 'invite.ics', type: 'text/calendar' }], calendarInvitation: true,
    html: '<p>Join us</p>', text: 'Join us', hasBlockedRemoteImages: false, remoteImages: true,
  } }));
  await page.route('**/api/calendar/invitations/*', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { invitation: {
      method: 'REQUEST', uid: 'meeting-1', summary: 'Planning', organizer: 'mailto:boss@example.test',
      startsAt: '2026-09-11T13:00:00.000Z', endsAt: '2026-09-11T14:00:00.000Z', allDay: false, localEvent: null, response: null,
    } } });
  });
  await page.route('**/api/calendar/events**', route => route.fulfill({ json: { events: [
    { id: 'standup', summary: 'Standup', starts_at: '2026-09-11T13:30:00Z', ends_at: '2026-09-11T13:45:00Z', calendar_id: 'calendar-personal', calendar_name: 'Personal' },
    { id: 'lunch', summary: 'Lunch', starts_at: '2026-09-11T12:00:00Z', ends_at: '2026-09-11T13:00:00Z', calendar_id: 'calendar-personal' },
  ] } }));
  const replies: unknown[] = [];
  await page.route('**/api/calendar/invitations/*/respond', async route => {
    replies.push(route.request().postDataJSON());
    await route.fulfill({ json: { response: 'ACCEPTED', localEvent: { calendarId: 'calendar-personal' } } });
  });
  const archived: string[] = [];
  await page.route('**/api/mail/conversations/*/archive', async route => {
    archived.push(new URL(route.request().url()).pathname);
    await route.fulfill({ json: { ok: true, action: 'archive', affectedIds: [], affectedCount: 1 } });
  });

  await page.goto('/?list=0&reader=1');
  await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
  const card = page.locator('[data-testid="calendar-invitation-card"]:visible').first();
  await expect(card).toBeVisible();

  // Only the overlapping event is listed; the one that ends as the invitation starts is not.
  await expect(card.locator('[data-testid="calendar-invitation-conflict"]')).toHaveCount(1);
  await expect(card.locator('[data-testid="calendar-invitation-conflict"]')).toContainText('Standup');

  await card.locator('[data-testid="calendar-invitation-accepted"]').click();
  await expect.poll(() => replies).toEqual([{ response: 'ACCEPTED', calendarId: 'calendar-personal' }]);
  await expect.poll(() => archived.length).toBe(1);
});
