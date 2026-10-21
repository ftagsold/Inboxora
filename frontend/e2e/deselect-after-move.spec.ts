import { test, expect } from './fixtures.ts';

// A message that leaves the folder leaves nothing selected — the reader closes rather than
// jumping to a neighbouring message the user did not choose.
test('archiving from the conversation reader clears the selection', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
  await fixtureApi;
  page.__conversationMatrix = '01';
  await page.route('**/api/mail/conversations/*/archive', route => route.fulfill({ json: { ok: true, action: 'archive', affectedIds: ['conversation-gmail-copy-1'], affectedCount: 1 } }));
  await page.goto('/?list=0&reader=1');
  await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
  const reader = page.locator('section[data-conversation-id]:visible');
  await expect(reader).toBeVisible();
  await reader.locator('[data-conversation-message-state="expanded"] button[data-message-action="archive"]').first().click();
  await expect(page.locator('section[data-conversation-id]:visible')).toHaveCount(0);
  await expect(page.locator('[data-msgid][aria-selected="true"]:visible, [data-msgid][data-selected="true"]:visible')).toHaveCount(0);
});
