import { test, expect } from './fixtures.ts';

const BODY = '<p>Current answer</p><div class="gmail_quote"><div>On Tue, Sep 1, 2026 Sender &lt;sender@gmail.test&gt; wrote:</div><blockquote class="gmail_quote"><p>Old quoted text</p></blockquote></div>';

async function open(page, fixtureApi, reader: boolean) {
  await fixtureApi;
  page.__conversationMatrix = `0${Number(reader)}`;
  await page.route('**/api/mail/messages/*/body**', route => route.fulfill({ json: { html: BODY, text: '', attachments: [], hasBlockedRemoteImages: false, remoteImages: true } }));
  await page.goto(`/?list=0&reader=${Number(reader)}`);
  await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
}

test('messages inside a multi-message conversation omit quoted history entirely', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
  await open(page, fixtureApi, true);
  const frame = page.locator('section[data-conversation-id] [data-message-detail-body] iframe').first().contentFrame();
  await expect(frame.locator('body')).toContainText('Current answer');
  await expect(frame.locator('body')).not.toContainText('Old quoted text');
  await expect(frame.locator('[data-mailflow-quote-toggle], .mailflow-quote-toggle')).toHaveCount(0);
});

test('a single message outside the conversation reader still folds its quote behind a toggle', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
  await open(page, fixtureApi, false);
  const frame = page.locator('[data-message-detail-body] iframe').first().contentFrame();
  await expect(frame.locator('body')).toContainText('Current answer');
  await expect(frame.locator('[data-mailflow-quote-group]').first()).toBeAttached();
});
