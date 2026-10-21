import { test, expect } from './fixtures.ts';

const readerOrder = (page) => page.locator('section[data-conversation-id="conversation-gmail"]:visible article[data-logical-message-id]')
  .evaluateAll(nodes => nodes.map(node => node.getAttribute('data-logical-message-id')));

test('a conversation shows its newest message on top by default', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
  await fixtureApi;
  page.__conversationMatrix = '11';
  await page.goto('/?list=1&reader=1');
  await page.locator('[data-msgid="conversation-gmail-copy-5"]:visible').click();
  await expect(page.locator('section[data-conversation-id="conversation-gmail"]:visible')).toBeVisible();
  const order = await readerOrder(page);
  expect(order[0]).toBe('conversation-gmail-logical-5');
  expect(order.at(-1)).toBe('conversation-gmail-logical-1');
});

test('the oldest-first preference restores chronological order', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
  await fixtureApi;
  page.__conversationMatrix = '11';
  page.__preferencesOverride = { conversationNewestFirst: false };
  await page.goto('/?list=1&reader=1');
  await page.locator('[data-msgid="conversation-gmail-copy-5"]:visible').click();
  await expect(page.locator('section[data-conversation-id="conversation-gmail"]:visible')).toBeVisible();
  const order = await readerOrder(page);
  expect(order[0]).toBe('conversation-gmail-logical-1');
  expect(order.at(-1)).toBe('conversation-gmail-logical-5');
});
