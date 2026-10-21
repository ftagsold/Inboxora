import { test, expect } from './fixtures.ts';

test('conversation AI actions visibly render their streamed result', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop conversation action contract');
  await fixtureApi;
  page.__conversationMatrix = '01';
  page.__preferencesOverride = {
    aiActions: [{ id: 'fixture-ai', label: 'Fixture AI action', prompt: 'Reply with the fixture result.' }],
  };
  // A configured provider; routes registered later take precedence over the fixture's default.
  await page.route('**/api/ai/status', route => route.fulfill({ json: { enabled: true, features: { summarize: true, compose: true } } }));
  await page.route('**/api/ai/chat', route => route.fulfill({
    contentType: 'text/event-stream',
    body: 'data: {"choices":[{"delta":{"content":"Visible AI fixture result"}}]}\n\ndata: [DONE]\n\n',
  }));

  await page.goto('/?list=0&reader=1');
  await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
  const reader = page.locator('section[data-conversation-id]:visible');
  const actionButton = reader.locator('button[data-message-action="ai"]').first();
  await expect(actionButton).toBeVisible();
  await actionButton.click();
  await page.getByRole('button', { name: 'Fixture AI action' }).click();

  const result = reader.getByTestId('conversation-ai-result');
  await expect(result).toContainText('Visible AI fixture result');
});

test('no AI entry is offered when no AI provider is configured', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop conversation action contract');
  await fixtureApi; // the fixture reports { enabled: false }
  page.__conversationMatrix = '01';
  page.__preferencesOverride = {
    aiActions: [{ id: 'fixture-ai', label: 'Fixture AI action', prompt: 'Reply with the fixture result.' }],
  };

  await page.goto('/?list=0&reader=1');
  await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
  const reader = page.locator('section[data-conversation-id]:visible');
  await expect(reader.locator('[data-conversation-message-actions="true"]').first()).toBeVisible();
  await expect(reader.locator('button[data-message-action="ai"]')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Fixture AI action' })).toHaveCount(0);
});
