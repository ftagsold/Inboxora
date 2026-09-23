import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: dragging a timed event keeps its original time in the preview', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  // "Planowanie projektu" starts at 08:00 per the v3 fixture (2026-09-10T08:00:00Z, Europe
  // timezone in the test env).
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const originalTime = (await event.textContent()) ?? '';
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 100, box.y + box.height / 2, { steps: 10 });
  const preview = page.getByTestId('calendar-month-move-preview');
  await expect(preview).toBeVisible();
  const previewText = (await preview.textContent()) ?? '';
  await page.mouse.up();
  console.log('original', JSON.stringify(originalTime), 'preview', JSON.stringify(previewText));
  const timeOf = (text: string) => text.match(/\d{1,2}:\d{2}/)?.[0];
  expect(timeOf(previewText)).toBe(timeOf(originalTime));
  expect(timeOf(previewText)).not.toBe('02:00');
});
