import { test, expect } from './fixtures.ts';

// The conversation reader's "Always allow" buttons must save the allow-list entry and
// reload the message with its remote images, like the single-message pane does.
for (const [kind, entry] of [
  ['sender', { type: 'address', value: 'sender@gmail.test' }],
  ['domain', { type: 'domain', value: 'gmail.test' }],
] as const) {
  test(`always allow ${kind} in the conversation reader loads the images`, async ({ page, fixtureApi }, testInfo) => {
    test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop conversation reader contract');
    await fixtureApi;
    page.__conversationMatrix = '01';
    const saved: unknown[] = [];
    await page.route('**/api/auth/preferences/whitelist-add', async route => {
      saved.push(route.request().postDataJSON());
      await route.fulfill({ json: { ok: true } });
    });

    await page.goto('/?list=0&reader=1');
    await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
    const reader = page.locator('section[data-conversation-id]:visible');
    const banner = reader.locator('[data-message-detail-remote-images="true"]').first();
    await expect(banner).toBeVisible();

    const reload = page.waitForRequest(request => /\/api\/mail\/messages\/[^/]+\/body/.test(request.url())
      && new URL(request.url()).searchParams.get('remoteImages') === '1');
    const buttons = banner.locator('button');
    // Buttons: load images, allow sender, allow domain.
    await buttons.nth(kind === 'sender' ? 1 : 2).click();
    await reload;
    expect(saved).toEqual([entry]);
    await expect(reader.locator('[data-message-detail-remote-images="true"]')).toHaveCount(0);
  });
}
