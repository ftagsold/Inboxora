import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// The calendar's loading notice appears only for requests slower than 500 ms.
for (const [label, delayMs, expectNotice] of [['a fast', 150, false], ['a slow', 1500, true]] as const) {
  test(`calendar loading notice for ${label} events request`, async ({ page, fixtureApi }, testInfo) => {
    test.skip(testInfo.project.name !== 'chromium-desktop', 'timing contract');
    await fixtureApi; await setupV3(page);
    let seenNotice = false;
    await page.route('**/api/calendar/events**', async route => {
      if (route.request().method() !== 'GET') return route.fallback();
      await new Promise(resolve => setTimeout(resolve, delayMs));
      await route.fulfill({ json: { events: [] } });
    });
    await page.goto('/'); await navigateModule(page, 'calendar');
    const notice = page.locator('.calendar-notice[role="status"]', { hasText: 'Wczytywanie kalendarza' });
    const deadline = Date.now() + delayMs + 700;
    while (Date.now() < deadline) {
      if (await notice.count()) seenNotice = true;
      await page.waitForTimeout(40);
    }
    expect(seenNotice).toBe(expectNotice);
    await expect(notice).toHaveCount(0);
  });
}
