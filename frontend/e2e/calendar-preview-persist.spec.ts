import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: the preview stays visible until the save request actually completes', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  let releaseSave: (() => void) | null = null;
  await page.route('**/api/calendar/events/**', async route => {
    if (route.request().method() !== 'PATCH') return route.fallback();
    await new Promise<void>(resolve => { releaseSave = resolve; });
    return route.fulfill({ json: { ok: true } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 100, box.y + box.height / 2, { steps: 10 });
  const preview = page.getByTestId('calendar-month-move-preview');
  await expect(preview).toBeVisible();
  await page.mouse.up();
  // The save request is deliberately stuck (releaseSave not called yet) — the preview must
  // still be visible right now, not have flickered back to nothing.
  await page.waitForTimeout(200);
  await expect(preview).toBeVisible();
  expect(releaseSave).not.toBeNull();
  releaseSave!();
  await expect(preview).toHaveCount(0);
});
