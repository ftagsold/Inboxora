import { test, expect } from './fixtures.ts';

// Quoted history in a conversation message is folded behind a "Show quoted text" toggle and
// collapsed by default — for every common reply format, not only Gmail's markup.
const BODIES = {
  gmail: '<p>Current answer</p><div class="gmail_quote"><div>On Tue, Sep 1, 2026 Sender &lt;sender@gmail.test&gt; wrote:</div><blockquote class="gmail_quote"><p>Old quoted text</p></blockquote></div>',
  german: '<div>Current answer</div><div><br></div><div>Am 21.10.2026 um 10:00 schrieb Max Mustermann &lt;max@example.test&gt;:</div><div>Old quoted text</div>',
  outlook: '<div>Current answer</div><div><b>Von:</b> Max Mustermann &lt;max@example.test&gt;<br><b>Gesendet:</b> Dienstag, 21. Oktober 2026 10:00<br><b>An:</b> me@example.test<br><b>Betreff:</b> Planung</div><div>Old quoted text</div>',
  dated: '<div>Current answer</div><div><br></div><div>16.10.2026 19:57:53 Maria Tagsold &lt;maria@example.test&gt;:</div><blockquote style="margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex"><div>16.10.2026 16:35:45 Hotel &lt;hotel@example.test&gt;:</div><div>Old quoted text</div></blockquote>',
  wrapped: '<div>Current answer</div><div>On Tue, Oct 21, 2026 at 10:00 AM<br>Max Mustermann &lt;max@example.test&gt; wrote:</div><div>Old quoted text</div>',
};

for (const [format, html] of Object.entries(BODIES)) {
  test(`conversation messages fold ${format} quoted history, collapsed by default`, async ({ page, fixtureApi }, testInfo) => {
    test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
    await fixtureApi;
    page.__conversationMatrix = '01';
    await page.route('**/api/mail/messages/*/body**', route => route.fulfill({ json: { html, text: '', attachments: [], hasBlockedRemoteImages: false, remoteImages: true } }));
    await page.goto('/?list=0&reader=1');
    await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
    const frame = page.locator('section[data-conversation-id] [data-message-detail-body] iframe').first().contentFrame();
    await expect(frame.locator('body')).toContainText('Current answer');
    const toggle = frame.locator('.mailflow-quote-toggle');
    await expect(toggle).toHaveCount(1);
    await expect(toggle).toHaveAttribute('aria-expanded', 'false');
    await expect(frame.getByText('Old quoted text')).toBeHidden();
    await toggle.click();
    await expect(frame.getByText('Old quoted text')).toBeVisible();
  });
}

test('quoted history is never painted expanded before it folds (no flicker)', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop reader contract');
  await fixtureApi;
  page.__conversationMatrix = '01';
  // A slow image (a signature logo, say) delays the frame's load event, which is when folding
  // used to run — the quote stayed painted expanded until then.
  await page.route('https://images.example.test/logo.png', async route => {
    await new Promise(resolve => setTimeout(resolve, 800));
    await route.fulfill({ contentType: 'image/gif', body: Buffer.from('R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==', 'base64') });
  });
  const html = BODIES.dated.replace('<div>Current answer</div>', '<div>Current answer</div><img src="https://images.example.test/logo.png" width="40" height="20">');
  await page.route('**/api/mail/messages/*/body**', route => route.fulfill({ json: { html, text: '', attachments: [], hasBlockedRemoteImages: false, remoteImages: true } }));
  await page.goto('/?list=0&reader=1');
  // Watch every painted frame from before the click: a body frame that is visible while its
  // quoted text exists without a fold toggle would have been painted expanded.
  await page.evaluate(() => {
    const state = { expandedPaint: false, frames: 0 };
    Reflect.set(window, '__quotePaint', state);
    const check = () => {
      for (const frame of document.querySelectorAll<HTMLIFrameElement>('[data-message-detail-body] iframe')) {
        const doc = frame.contentDocument;
        if (!doc || doc.URL !== 'about:srcdoc' || getComputedStyle(frame).visibility === 'hidden') continue;
        const hasQuote = doc.body?.textContent?.includes('Old quoted text');
        if (hasQuote && !doc.querySelector('.mailflow-quote-toggle')) state.expandedPaint = true;
      }
      state.frames += 1;
      requestAnimationFrame(check);
    };
    requestAnimationFrame(check);
  });
  await page.locator('[data-msgid="conversation-gmail-copy-1"]:visible').click();
  const frame = page.locator('section[data-conversation-id] [data-message-detail-body] iframe').first().contentFrame();
  await expect(frame.locator('.mailflow-quote-toggle')).toHaveCount(1);
  const paint = await page.evaluate(() => Reflect.get(window, '__quotePaint') as { expandedPaint: boolean; frames: number });
  expect(paint.frames).toBeGreaterThan(0);
  expect(paint.expandedPaint).toBe(false);
});
