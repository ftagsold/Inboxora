import { test, expect } from './fixtures.ts';

// Quoted history in a conversation message is folded behind a "Show quoted text" toggle and
// collapsed by default — for every common reply format, not only Gmail's markup.
const BODIES = {
  gmail: '<p>Current answer</p><div class="gmail_quote"><div>On Tue, Sep 1, 2026 Sender &lt;sender@gmail.test&gt; wrote:</div><blockquote class="gmail_quote"><p>Old quoted text</p></blockquote></div>',
  german: '<div>Current answer</div><div><br></div><div>Am 21.10.2026 um 10:00 schrieb Max Mustermann &lt;max@example.test&gt;:</div><div>Old quoted text</div>',
  outlook: '<div>Current answer</div><div><b>Von:</b> Max Mustermann &lt;max@example.test&gt;<br><b>Gesendet:</b> Dienstag, 21. Oktober 2026 10:00<br><b>An:</b> me@example.test<br><b>Betreff:</b> Planung</div><div>Old quoted text</div>',
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
